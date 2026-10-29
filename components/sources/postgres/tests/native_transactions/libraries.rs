// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Build the three PostgreSQL packages with dynamic-plugin before running.
//! Dynamic cases call their actual native entry points, not Rust replacements.

use super::*;
use drasi_computation_plugin_sdk::{ConsumerMode, Scope};
use drasi_core::interface::FailureMode;
use drasi_host_sdk::computation::{self, NativeConsumerProxy, NativePlugin};
use drasi_lib::DrasiLib;
use futures::StreamExt;
use serde_json::{json, Value};
use std::path::PathBuf;

const INSTANCE: &str = "postgres-native-libraries";
const TIMEOUT: Duration = Duration::from_secs(20);

pub(super) struct Plugins {
    pub(super) source: Arc<NativePlugin>,
    pub(super) bootstrap: Arc<NativePlugin>,
    pub(super) sink: Arc<NativePlugin>,
}

fn load(package: &str) -> Result<Arc<NativePlugin>> {
    let directory = match std::env::var_os("DRASI_NATIVE_PLUGIN_DIR") {
        Some(directory) => PathBuf::from(directory),
        None => std::env::current_exe()?
            .parent()
            .context("test directory")?
            .parent()
            .context("debug directory")?
            .to_owned(),
    };
    let path = directory.join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        package.replace('-', "_"),
        std::env::consts::DLL_SUFFIX,
    ));
    anyhow::ensure!(
        path.is_file(),
        "missing {}; build {package} --features dynamic-plugin first",
        path.display()
    );
    computation::load(path)
}

pub(super) fn plugins(dynamic: bool) -> Result<Plugins> {
    let plugins = if dynamic {
        Plugins {
            source: load("drasi-computation-source-postgres")?,
            bootstrap: load("drasi-computation-bootstrap-postgres")?,
            sink: load("drasi-computation-sink-storedproc-postgres")?,
        }
    } else {
        Plugins {
            source: NativePlugin::from_definition(drasi_computation_source_postgres::plugin()?)?,
            bootstrap: NativePlugin::from_definition(
                drasi_computation_bootstrap_postgres::plugin()?,
            )?,
            sink: NativePlugin::from_definition(
                drasi_computation_sink_storedproc_postgres::plugin()?,
            )?,
        }
    };
    assert_eq!(
        plugins
            .source
            .factories()
            .iter()
            .map(|factory| factory.metadata().implementation.name.as_ref())
            .collect::<Vec<_>>(),
        [
            "drasi/postgres-transactions",
            "drasi.postgres/changes",
            "drasi.postgres/shared-changes",
            "drasi.postgres/shared-transactions"
        ],
    );
    assert!(plugins.source.bootstrap_factories().is_empty());
    assert!(plugins.bootstrap.factories().is_empty());
    assert_eq!(
        plugins
            .bootstrap
            .bootstrap_factories()
            .iter()
            .map(|factory| factory.metadata().implementation.name.as_ref())
            .collect::<Vec<_>>(),
        [
            "drasi.postgres/coordinated-bootstrap",
            "drasi.postgres/snapshot",
            "drasi.postgres/coordinated-row-bootstrap",
            "drasi.postgres/shared-row-bootstrap",
            "drasi.postgres/shared-transaction-bootstrap"
        ],
    );
    assert_eq!(plugins.sink.factories().len(), 1);
    assert!(plugins.sink.bootstrap_factories().is_empty());
    assert_eq!(
        plugins.sink.factories()[0].consumer_mode(),
        Some(ConsumerMode::External)
    );
    Ok(plugins)
}

pub(super) fn scope(graph: &str) -> Scope {
    Scope {
        instance_id: INSTANCE.into(),
        graph_id: graph.into(),
        generation: 1,
    }
}

pub(super) fn indexes(path: &Path) -> Result<Arc<dyn ComputationIndexProvider>> {
    Ok(Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
        ),
    )))
}

fn options() -> DeliveryOptions {
    DeliveryOptions {
        scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
        max_streams: size(4),
        receipts_per_stream: size(4),
        retry: DeliveryRetryPolicy {
            max_attempts: size(3),
            delay: Duration::from_millis(5),
        },
    }
}

fn sink_inputs() -> Result<Vec<InputBinding>> {
    Ok(vec![InputBinding {
        input_port: PortId::try_new("in")?,
        output: OutputEndpoint::new(ComponentId::try_new("query")?, PortId::try_new("out")?),
        stream: StreamId::try_new("results")?,
    }])
}

fn sink_settings(database: &Database) -> Value {
    let connection = &database.config.connection;
    json!({
        "hostname": connection.host, "port": connection.port,
        "database": connection.database, "user": connection.user, "password": connection.password,
        "ssl": connection.ssl_mode == SslMode::Require,
        "tls_ca_pem": database.config.tls_ca_pem,
        "command_timeout_ms": 5000, "connect_timeout_ms": 5000, "shutdown_timeout_ms": 5000,
        "default_template": {
            "added": {"template": "CALL native_upsert('ADD', {{param after.id}}, {{param after.name}})"},
            "updated": {"template": "CALL native_upsert('UPDATE', {{param after.id}}, {{param after.name}})"},
            "deleted": {"template": "CALL native_delete({{param before.id}}, {{param before.name}})"}
        }
    })
}

async fn prepare_destination(database: &Database) -> Result<()> {
    database
        .client()
        .batch_execute(
            "CREATE TABLE mirrored(id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         CREATE TABLE calls(operation TEXT NOT NULL, id INTEGER NOT NULL, name TEXT NOT NULL);
         CREATE TABLE failure_gate(enabled BOOLEAN NOT NULL, failed_id INTEGER NOT NULL);
         INSERT INTO failure_gate VALUES (false, 2);
         CREATE PROCEDURE native_upsert(op TEXT, row_id INTEGER, row_name TEXT)
         LANGUAGE plpgsql AS $$
         BEGIN
           IF (SELECT enabled AND failed_id=row_id FROM failure_gate) THEN
             RAISE EXCEPTION 'intentional second-operation failure';
           END IF;
           INSERT INTO mirrored VALUES (row_id, row_name)
             ON CONFLICT (id) DO UPDATE SET name=EXCLUDED.name;
           INSERT INTO calls VALUES(op, row_id, row_name);
         END $$;
         CREATE PROCEDURE native_delete(row_id INTEGER, row_name TEXT)
         LANGUAGE plpgsql AS $$
         BEGIN
           DELETE FROM mirrored WHERE id=row_id;
           INSERT INTO calls VALUES('DELETE', row_id, row_name);
         END $$;",
        )
        .await?;
    Ok(())
}

async fn sink(
    plugin: &NativePlugin,
    database: &Database,
    path: &Path,
    graph: &str,
) -> Result<NativeConsumerProxy> {
    plugin.factories()[0]
        .create_consumer_with_inputs(
            ComponentId::try_new("sink")?,
            json!({"settings": sink_settings(database)}),
            scope(graph),
            indexes(path)?,
            options(),
            sink_inputs()?,
        )
        .await
}

async fn open(
    plugins: &Plugins,
    database: &Database,
    path: &Path,
) -> Result<(DrasiLib, Arc<QuerySourceProgress>, QueryResults)> {
    let progress = Arc::new(QuerySourceProgress::new(
        ComponentBatch::GRAPH_ID,
        ComponentId::try_new("query")?,
    )?);
    let bootstrap = plugins.bootstrap.bootstrap_factories()[0].create_provider(
        ComponentId::try_new("query")?,
        json!({"source": "source", "stream": "changes", "settings": database.config}),
        Some(scope(ComponentBatch::GRAPH_ID)),
        Some(progress.clone()),
    )?;
    let query = query(path.join("query").as_path(), progress.clone())
        .await?
        .with_recovery_scope(INSTANCE)?
        .with_bootstrap(bootstrap);
    let results = query.results();
    let sink = sink(
        &plugins.sink,
        database,
        &path.join("consumer"),
        ComponentBatch::GRAPH_ID,
    )
    .await?;
    let factory = plugins.source.factories()[0].clone();
    let resource = ResourceId::try_new("progress")?;
    let settings = ResourceId::try_new("settings")?;
    let specification = ComponentSpecification {
        descriptor: factory
            .metadata()
            .descriptor(ComponentId::try_new("source")?)?,
        role: ComponentRole::Source,
        completion: None,
        implementation: factory.descriptor().implementation.clone(),
        configuration_version: 1,
        configuration: BTreeMap::from([
            (
                "stream".into(),
                ConfigurationValue::Literal(json!("changes")),
            ),
            (
                "coordinated_snapshot".into(),
                ConfigurationValue::Literal(json!(true)),
            ),
            (
                "settings".into(),
                ConfigurationValue::Reference {
                    resource: settings.clone(),
                    key: "postgres".into(),
                    secret: true,
                },
            ),
        ]),
        dependencies: BTreeMap::from([("source_progress".into(), vec![resource.clone()])]),
    };
    let batch = ComponentBatch::builder()
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::Checkpoint,
            ownership: ResourceOwnership::Borrowed,
            binding: "progress".into(),
        })?
        .provide_resource(
            resource,
            ResourceHandle::new(
                ResourceRole::Checkpoint,
                Arc::new(QuerySourceProgressResource(progress.clone())),
            ),
        )?
        .declare_resource(ResourceSpecification {
            id: settings.clone(),
            role: ResourceRole::SecretStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "settings".into(),
        })?
        .provide_resource(
            settings,
            ResourceHandle::new(
                ResourceRole::SecretStore,
                Arc::new(ConfigurationResolverResource(Arc::new(Settings(
                    serde_json::to_value(&database.config)?,
                )))),
            ),
        )?
        .component(specification, factory)
        .query(Box::new(query))
        .sink(Box::new(sink))
        .bind_stream(endpoint("source", "out"), StreamId::try_new("changes")?)
        .bind_stream(endpoint("query", "out"), StreamId::try_new("results")?)
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("query", "out"), endpoint("sink", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()?;
    let core = DrasiLib::builder()
        .with_id(INSTANCE)
        .with_components(batch)
        .build()
        .await?;
    let started = async {
        core.start().await?;
        for id in ["query", "source", "sink"] {
            tokio::time::timeout(TIMEOUT, core.computation_component(id)?.wait_started()).await??;
        }
        Result::<()>::Ok(())
    }
    .await;
    if let Err(error) = started {
        core.shutdown()
            .await
            .context("cleanup after rejected native PostgreSQL graph")?;
        return Err(error);
    }
    assert_eq!(
        core.computation_control()?.desired_snapshot().id.as_ref(),
        ComponentBatch::GRAPH_ID
    );
    Ok((core, progress, results))
}

async fn count(database: &Database) -> Result<i64> {
    Ok(database
        .client()
        .query_one("SELECT count(*) FROM calls", &[])
        .await?
        .get(0))
}

async fn wait_mirrored(database: &Database, expected: &[(i32, &str)], calls: i64) -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let rows = database
                .client()
                .query("SELECT id, name FROM mirrored ORDER BY id", &[])
                .await?;
            let actual = rows
                .iter()
                .map(|row| (row.get::<_, i32>(0), row.get::<_, String>(1)))
                .collect::<Vec<_>>();
            let expected = expected
                .iter()
                .map(|(id, name)| (*id, name.to_string()))
                .collect::<Vec<_>>();
            let count = count(database).await?;
            anyhow::ensure!(count <= calls, "native stored procedure repeated an effect");
            if actual == expected && count == calls {
                return Result::<()>::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("native PostgreSQL pipeline did not deliver expected rows")?
}

async fn end_to_end() -> Result<()> {
    for (dynamic, tls) in [(false, false), (true, false), (true, true)] {
        let plugins = plugins(dynamic)?;
        let database = if tls {
            tls::Certificates::new(false)?.database().await?
        } else {
            Database::new().await?
        };
        prepare_destination(&database).await?;
        let directory = tempfile::tempdir()?;
        let (core, progress, results) = open(&plugins, &database, directory.path()).await?;
        let slot = snapshot::managed_slot(&database).await?;
        database
            .client()
            .batch_execute(
                "BEGIN; INSERT INTO person VALUES(1, 'first');
             UPDATE person SET name='final' WHERE id=1;
             INSERT INTO person VALUES(2, 'temporary'); DELETE FROM person WHERE id=2; COMMIT;",
            )
            .await?;
        wait_mirrored(&database, &[(1, "final")], 1).await?;
        assert_eq!(results.snapshot()?.rows.len(), 1);
        database
            .wait_slot_feedback(&slot, committed(&progress))
            .await?;
        let position: Value = serde_json::from_slice(
            progress.snapshot().checkpoints[&SourceProgressKey::Source("source".into())]
                .source_position
                .as_ref()
                .context("source cursor")?,
        )?;
        assert_eq!(position["managed_slot"], slot);
        if tls {
            let encrypted: bool = database.client().query_one(
                "SELECT bool_and(s.ssl) FROM pg_stat_ssl s JOIN pg_stat_activity a USING(pid)
                 WHERE a.backend_type='walsender' OR a.application_name='drasi-native-storedproc-postgres'",
                &[],
            ).await?.get(0);
            assert!(encrypted, "native source and sink must verify and use TLS");
        }
        let hostile = "007'); DROP TABLE person; --";
        database
            .client()
            .execute("UPDATE person SET name=$1 WHERE id=1", &[&hostile])
            .await?;
        wait_mirrored(&database, &[(1, hostile)], 2).await?;
        core.shutdown().await?;
        drop((core, results, progress));
        tls::retired(&database).await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES(3, 'offline')")
            .await?;
        let (core, progress, results) = open(&plugins, &database, directory.path()).await?;
        assert_eq!(snapshot::managed_slot(&database).await?, slot);
        wait_mirrored(&database, &[(1, hostile), (3, "offline")], 3).await?;
        assert_eq!(results.snapshot()?.rows.len(), 2);
        database
            .wait_slot_feedback(&slot, committed(&progress))
            .await?;
        database
            .client()
            .batch_execute("DELETE FROM person WHERE id=1")
            .await?;
        wait_mirrored(&database, &[(3, "offline")], 4).await?;
        core.shutdown().await?;
        drop((core, results, progress));
        tls::retired(&database).await?;
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_libraries_share_one_instance_graph_and_recover_current_thread() -> Result<()> {
    end_to_end().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_libraries_share_one_instance_graph_and_recover_multithread() -> Result<()> {
    end_to_end().await
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_rejects_a_same_named_but_different_actual_owner() -> Result<()> {
    let plugins = plugins(true)?;
    let database = Database::new().await?;
    let expected = progress();
    let other = progress();
    let snapshot = plugins.bootstrap.bootstrap_factories()[0].create_provider(
        ComponentId::try_new("query")?,
        json!({"source":"source","stream":"changes","settings": database.config}),
        Some(scope(expected.graph_id())),
        Some(expected.clone()),
    )?;
    let path = tempfile::tempdir()?;
    let mut query = query(path.path(), other)
        .await?
        .with_recovery_scope(INSTANCE)?
        .with_bootstrap(snapshot);
    let failure = query.start().await.expect_err("different actual owner");
    assert!(
        format!("{failure:#}").contains("actual progress owner"),
        "{failure:#}"
    );
    query.stop().await?;
    assert!(expected.snapshot().checkpoints.is_empty());
    let slots: i64 = database
        .client()
        .query_one(
            "SELECT count(*) FROM pg_replication_slots WHERE slot_name LIKE 'native_slot\\_%'",
            &[],
        )
        .await?
        .get(0);
    assert_eq!(slots, 0, "owner rejection must precede snapshot effects");
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn native_storedproc_resumes_partial_batches_and_deduplicates_destination_receipts(
) -> Result<()> {
    let plugins = plugins(true)?;
    let database = Database::new().await?;
    prepare_destination(&database).await?;
    database
        .client()
        .batch_execute("UPDATE failure_gate SET enabled=true")
        .await?;
    let directory = tempfile::tempdir()?;
    let owner = progress();
    let mut query = query(&directory.path().join("query"), owner.clone()).await?;
    query.start().await?;
    let mut source = source(database.config.clone(), owner)?;
    source.start().await?;
    database
        .client()
        .batch_execute("INSERT INTO person VALUES(1, 'one'), (2, 'two')")
        .await?;
    let group = next(&mut source).await?;
    let outputs = query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: group.envelope,
        })
        .await?;
    let ChangeOperation::Added { after, .. } = &outputs[0].envelope.changes().operations()[1]
    else {
        anyhow::bail!("expected two added query rows");
    };
    let row = QueryChangeCodec::decode_row(after)?;
    let failed_id = i32::try_from(
        QueryChangeCodec::row_values_to_json(&row.values)["id"]
            .as_i64()
            .context("second operation ID")?,
    )?;
    database
        .client()
        .execute("UPDATE failure_gate SET failed_id=$1", &[&failed_id])
        .await?;
    let input = || InputEnvelope {
        port: PortId::try_new("in").unwrap(),
        envelope: outputs[0].envelope.clone(),
    };
    let path = directory.path().join("consumer");
    let mut consumer = sink(&plugins.sink, &database, &path, "native-postgres").await?;
    consumer.start().await?;
    assert!(consumer.handle(input()).await.is_err());
    assert_eq!(count(&database).await?, 1);
    assert_eq!(consumer.progress().await?[0].completed_operations, 1);
    consumer.stop().await?;
    drop(consumer);
    database
        .client()
        .batch_execute("UPDATE failure_gate SET enabled=false")
        .await?;
    let mut consumer = sink(&plugins.sink, &database, &path, "native-postgres").await?;
    consumer.start().await?;
    consumer.handle(input()).await?;
    assert_eq!(count(&database).await?, 2);
    assert_eq!(consumer.progress().await?[0].completed_operations, 2);
    consumer.stop().await?;
    drop(consumer);

    // A fresh local receipt owner models recovery after a lost local response;
    // the existing destination receipt remains authoritative and is not deleted.
    let mut duplicate = sink(
        &plugins.sink,
        &database,
        &directory.path().join("unconfirmed-local-receipt"),
        "native-postgres",
    )
    .await?;
    duplicate.start().await?;
    duplicate.handle(input()).await?;
    assert_eq!(
        count(&database).await?,
        2,
        "committed destination effects must not repeat"
    );
    duplicate.stop().await?;
    drop(duplicate);
    query.delivery_completed(&outputs).await?;
    source.stop().await?;
    query.stop().await?;
    database.shutdown().await
}

async fn captured(
    database: &Database,
    path: &Path,
    statement: &str,
) -> Result<(ContinuousQueryTransformer, Vec<OutputEnvelope>)> {
    let owner = progress();
    let mut query = query(path, owner.clone()).await?;
    query.start().await?;
    let mut source = source(database.config.clone(), owner)?;
    source.start().await?;
    database.client().batch_execute(statement).await?;
    let group = next(&mut source).await?;
    let outputs = query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: group.envelope,
        })
        .await?;
    source.stop().await?;
    Ok((query, outputs))
}

#[tokio::test(flavor = "current_thread")]
async fn native_storedproc_retries_only_rolled_back_retryable_sql_outcomes() -> Result<()> {
    let plugins = plugins(true)?;
    let database = Database::new().await?;
    prepare_destination(&database).await?;
    database
        .client()
        .batch_execute(
            "CREATE SEQUENCE retry_attempt;
         CREATE OR REPLACE PROCEDURE native_upsert(op TEXT, row_id INTEGER, row_name TEXT)
         LANGUAGE plpgsql AS $$
         BEGIN
           IF nextval('retry_attempt') <= 2 THEN
             RAISE EXCEPTION USING MESSAGE='injected serialization failure', ERRCODE='40001';
           END IF;
           INSERT INTO mirrored VALUES (row_id, row_name);
           INSERT INTO calls VALUES(op, row_id, row_name);
         END $$;",
        )
        .await?;
    let directory = tempfile::tempdir()?;
    let (mut query, outputs) = captured(
        &database,
        &directory.path().join("query"),
        "INSERT INTO person VALUES(1, 'retried')",
    )
    .await?;
    let mut consumer = sink(
        &plugins.sink,
        &database,
        &directory.path().join("consumer"),
        "native-postgres",
    )
    .await?;
    consumer.start().await?;
    consumer
        .handle(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: outputs[0].envelope.clone(),
        })
        .await?;
    assert_eq!(count(&database).await?, 1);
    let attempts: i64 = database
        .client()
        .query_one("SELECT last_value FROM retry_attempt", &[])
        .await?
        .get(0);
    assert_eq!(
        attempts, 3,
        "the host must preserve the native handler's retry classification"
    );
    assert_eq!(consumer.progress().await?[0].completed_operations, 1);
    consumer.stop().await?;
    drop(consumer);
    query.delivery_completed(&outputs).await?;
    query.stop().await?;
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn native_storedproc_cancellation_rolls_back_and_requires_joined_cleanup() -> Result<()> {
    let plugins = plugins(true)?;
    let database = Database::new().await?;
    prepare_destination(&database).await?;
    database
        .client()
        .batch_execute(
            "CREATE OR REPLACE PROCEDURE native_upsert(op TEXT, row_id INTEGER, row_name TEXT)
         LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO mirrored VALUES (row_id, row_name);
           INSERT INTO calls VALUES(op, row_id, row_name);
           PERFORM pg_sleep(30);
         END $$;",
        )
        .await?;
    let directory = tempfile::tempdir()?;
    let (mut query, outputs) = captured(
        &database,
        &directory.path().join("query"),
        "INSERT INTO person VALUES(1, 'cancelled')",
    )
    .await?;
    let input = || InputEnvelope {
        port: PortId::try_new("in").unwrap(),
        envelope: outputs[0].envelope.clone(),
    };
    let path = directory.path().join("consumer");
    let mut settings = sink_settings(&database);
    settings["command_timeout_ms"] = json!(1000);
    let mut consumer = plugins.sink.factories()[0]
        .create_consumer_with_inputs(
            ComponentId::try_new("sink")?,
            json!({"settings": settings}),
            scope("native-postgres"),
            indexes(&path)?,
            options(),
            sink_inputs()?,
        )
        .await?;
    consumer.start().await?;
    tokio::time::timeout(TIMEOUT, async {
        tokio::select! {
            result = consumer.handle(input()) => anyhow::bail!("delivery completed before cancellation: {result:?}"),
            observed = async {
                loop {
                    let sleeping: bool = database.client().query_one(
                        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity
                         WHERE application_name='drasi-native-storedproc-postgres'
                         AND wait_event='PgSleep')", &[],
                    ).await?.get(0);
                    if sleeping { return Result::<()>::Ok(()); }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            } => observed,
        }
    }).await??;
    assert_eq!(
        count(&database).await?,
        0,
        "uncommitted writes must remain invisible"
    );
    assert_eq!(consumer.progress().await?[0].completed_operations, 0);
    assert!(
        consumer.start().await.is_err(),
        "cancellation must not authorize replacement"
    );
    consumer.stop().await?;
    drop(consumer);
    tls::retired(&database).await?;
    assert_eq!(
        count(&database).await?,
        0,
        "cancelled effect must roll back before cleanup ends"
    );
    database
        .client()
        .batch_execute(
            "CREATE OR REPLACE PROCEDURE native_upsert(op TEXT, row_id INTEGER, row_name TEXT)
         LANGUAGE plpgsql AS $$
         BEGIN
           INSERT INTO mirrored VALUES (row_id, row_name);
           INSERT INTO calls VALUES(op, row_id, row_name);
         END $$;",
        )
        .await?;
    let mut consumer = sink(&plugins.sink, &database, &path, "native-postgres").await?;
    consumer.start().await?;
    consumer.handle(input()).await?;
    assert_eq!(count(&database).await?, 1);
    consumer.stop().await?;
    drop(consumer);
    query.delivery_completed(&outputs).await?;
    query.stop().await?;
    database.shutdown().await
}

struct PausedBootstrap {
    inner: Arc<dyn ComputationBootstrapProvider>,
    prepared: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    timestamps: Arc<std::sync::Mutex<Vec<u64>>>,
}

#[async_trait::async_trait]
impl ComputationBootstrapProvider for PausedBootstrap {
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.inner.recovery_reader()
    }
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        self.inner.validate_query_scope(instance, graph, query)
    }
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.inner.prepare_with_state(state).await
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        anyhow::bail!("coordinated state required")
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        let snapshot = self.inner.snapshot_with_state(state).await?;
        self.prepared.notify_one();
        let release = self.release.clone();
        let timestamps = self.timestamps.clone();
        Ok(ComputationBootstrapSnapshot {
            watermarks: snapshot.watermarks,
            changes: Box::pin(
                futures::stream::once(async move {
                    release.notified().await;
                    snapshot.changes.map(move |envelope| {
                        let envelope = envelope?;
                        for change in GraphChangeCodec::decode_changes(&envelope)? {
                            timestamps
                                .lock()
                                .map_err(|_| anyhow::anyhow!("snapshot observations poisoned"))?
                                .push(change.get_transaction_time());
                        }
                        Result::<_>::Ok(envelope)
                    })
                })
                .flatten(),
            ),
        })
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        self.inner.complete_snapshot().await
    }
    fn completion_state(&self) -> Result<Option<bytes::Bytes>> {
        self.inner.completion_state()
    }
    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }
}

fn bootstrap(
    plugins: &Plugins,
    database: &Database,
    owner: Arc<QuerySourceProgress>,
) -> Result<Arc<dyn ComputationBootstrapProvider>> {
    Ok(plugins.bootstrap.bootstrap_factories()[0].create_provider(
        ComponentId::try_new("query")?,
        json!({"source": "source", "stream": "changes", "settings": database.config}),
        Some(scope(owner.graph_id())),
        Some(owner),
    )?)
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_cancellation_preserves_intent_and_reset_replaces_only_its_unfinished_slot(
) -> Result<()> {
    let plugins = plugins(true)?;
    let database = Database::new().await?;
    database
        .client()
        .batch_execute("INSERT INTO person VALUES(1, 'initial')")
        .await?;
    let directory = tempfile::tempdir()?;
    let abandoned;
    {
        let owner = progress();
        let prepared = Arc::new(tokio::sync::Notify::new());
        let mut query = query(directory.path(), owner.clone())
            .await?
            .with_recovery_scope(INSTANCE)?
            .with_bootstrap(Arc::new(PausedBootstrap {
                inner: bootstrap(&plugins, &database, owner.clone())?,
                prepared: prepared.clone(),
                release: Arc::new(tokio::sync::Notify::new()),
                timestamps: Arc::default(),
            }));
        tokio::time::timeout(TIMEOUT, async {
            tokio::select! {
                result = query.start() => anyhow::bail!("snapshot completed before cancellation: {result:?}"),
                _ = prepared.notified() => Result::<()>::Ok(()),
            }
        }).await??;
        assert!(owner.snapshot().checkpoints.is_empty());
        assert!(!owner.snapshot().bootstrap_complete);
        abandoned = snapshot::managed_slot(&database).await?;
        query.stop().await?;
    }
    tls::retired(&database).await?;
    database
        .client()
        .batch_execute("UPDATE person SET name='after interruption'")
        .await?;
    {
        let owner = progress();
        let mut query = query(directory.path(), owner.clone())
            .await?
            .with_recovery_scope(INSTANCE)?
            .with_bootstrap(bootstrap(&plugins, &database, owner)?);
        let error = query
            .start()
            .await
            .expect_err("strict recovery retains incomplete initialization");
        assert!(matches!(
            error.downcast_ref::<QueryRecoveryError>(),
            Some(QueryRecoveryError::IncompleteBootstrap)
        ));
        query.stop().await?;
        assert_eq!(snapshot::managed_slot(&database).await?, abandoned);
    }
    {
        let owner = progress();
        let mut query = query_with_recovery(
            directory.path(),
            owner.clone(),
            QueryRecoveryPolicy::AutoReset,
        )
        .await?
        .with_recovery_scope(INSTANCE)?
        .with_bootstrap(bootstrap(&plugins, &database, owner.clone())?);
        query.start().await?;
        assert_ne!(snapshot::managed_slot(&database).await?, abandoned);
        assert_eq!(
            rows(&query),
            vec![json!({"id":1,"name":"after interruption"})]
        );
        assert!(owner.snapshot().bootstrap_complete);
        query.stop().await?;
    }
    let external: bool = database
        .client()
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_replication_slots WHERE slot_name='native_slot')",
            &[],
        )
        .await?
        .get(0);
    assert!(external, "an externally managed slot must never be removed");
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn native_bootstrap_commits_exact_snapshot_before_concurrent_wal() -> Result<()> {
    let plugins = plugins(true)?;
    let database = Database::new().await?;
    database
        .client()
        .batch_execute("INSERT INTO person VALUES(1, 'before'), (2, 'deleted')")
        .await?;
    let directory = tempfile::tempdir()?;
    let owner = progress();
    let prepared = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let timestamps = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut query = query(directory.path(), owner.clone())
        .await?
        .with_recovery_scope(INSTANCE)?
        .with_bootstrap(Arc::new(PausedBootstrap {
            inner: bootstrap(&plugins, &database, owner.clone())?,
            prepared: prepared.clone(),
            release: release.clone(),
            timestamps: timestamps.clone(),
        }));
    let (started, written) = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(query.start(), async {
            prepared.notified().await;
            database
                .client()
                .batch_execute(
                    "BEGIN; UPDATE person SET name='during' WHERE id=1;
                 DELETE FROM person WHERE id=2; INSERT INTO person VALUES(3,'live'); COMMIT;",
                )
                .await?;
            assert!(owner.snapshot().checkpoints.is_empty());
            release.notify_one();
            Result::<()>::Ok(())
        })
    })
    .await?;
    started?;
    written?;
    assert_eq!(
        rows(&query),
        vec![
            json!({"id":1,"name":"before"}),
            json!({"id":2,"name":"deleted"})
        ]
    );
    let (mut source, _) = PostgresTransactionSource::coordinated(
        ComponentId::try_new("source")?,
        StreamId::try_new("changes")?,
        database.config.clone(),
        owner,
    )?;
    source.start().await?;
    let group = next(&mut source).await?;
    let changes = SourceTransactionCodec::decode(&group.envelope, limits(16))?.into_changes();
    let latest_initial = *timestamps
        .lock()
        .unwrap()
        .iter()
        .max()
        .context("snapshot timestamps")?;
    let earliest_live = changes
        .iter()
        .map(|change| change.get_transaction_time())
        .min()
        .context("live transaction timestamp")?;
    assert!(
        earliest_live >= latest_initial,
        "snapshot timestamp {latest_initial} is ahead of its later WAL transaction {earliest_live}"
    );
    apply(&mut query, group).await?;
    assert_eq!(
        rows(&query),
        vec![
            json!({"id":1,"name":"during"}),
            json!({"id":3,"name":"live"})
        ]
    );
    source.stop().await?;
    query.stop().await?;
    database.shutdown().await
}
