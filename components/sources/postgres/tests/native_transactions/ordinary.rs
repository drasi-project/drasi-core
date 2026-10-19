// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_sdk::{
    NativeComponent as PluginComponent, NativeSource as PluginSource,
};
use drasi_core::computation::InMemoryComputationProvider;
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_lib::DrasiLib;
use drasi_postgres_common::replication::rows::RowPosition;
use futures::{StreamExt, TryStreamExt};
use serde_json::{json, Value};

const INSTANCE: &str = "postgres-native-libraries";

struct Ingress {
    descriptor: ComponentDescriptor,
    receiver: tokio::sync::mpsc::Receiver<OutputEnvelope>,
}
#[async_trait::async_trait]
impl ComputationComponent for Ingress {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<Value> {
        Ok(json!({}))
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait::async_trait]
impl EnvelopeSource for Ingress {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self.receiver.recv().await)
    }
}

async fn row_graph(
    plugins: &libraries::Plugins,
    database: &Database,
    persistent: Option<&Path>,
    coordinated: bool,
) -> Result<(
    DrasiLib,
    Arc<QuerySourceProgress>,
    QueryResults,
    tokio::sync::mpsc::Receiver<InputEnvelope>,
)> {
    let progress = Arc::new(QuerySourceProgress::new(
        ComponentBatch::GRAPH_ID,
        ComponentId::try_new("query")?,
    )?);
    let indexes: Arc<dyn ComputationIndexProvider> = match persistent {
        Some(path) => libraries::indexes(path)?,
        None => Arc::new(InMemoryComputationProvider),
    };
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: ComponentBatch::GRAPH_ID.into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:person) RETURN n.id AS id, n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("results")?,
            outbox_capacity: size(16),
        },
        indexes,
    )
    .await?
    .with_source_progress(progress.clone())?
    .with_recovery_scope(INSTANCE)?;
    if coordinated {
        let factory = plugins
            .bootstrap
            .bootstrap_factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == drasi_computation_bootstrap_postgres::POSTGRES_ROW_BOOTSTRAP
            })
            .context("row bootstrap factory")?;
        query = query.with_bootstrap(factory.create_provider(
            ComponentId::try_new("query")?,
            json!({"source":"source", "stream":"changes", "settings":database.config}),
            Some(libraries::scope(ComponentBatch::GRAPH_ID)),
            Some(progress.clone()),
        )?);
    }
    let results = query.results();
    let (sink, received) = drasi_reaction_application::NativeApplicationReaction::channel(
        "sink",
        QueryChangeCodec::schema().descriptor().clone(),
        size(8),
    )?;
    let factory = plugins
        .source
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == drasi_computation_source_postgres::POSTGRES_ROWS
        })
        .context("ordinary row factory")?
        .clone();
    let owner = ResourceId::try_new("progress")?;
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
                "persistent".into(),
                ConfigurationValue::Literal(json!(persistent.is_some())),
            ),
            (
                "coordinated_snapshot".into(),
                ConfigurationValue::Literal(json!(coordinated)),
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
        dependencies: BTreeMap::from([("source_progress".into(), vec![owner.clone()])]),
    };
    let batch = ComponentBatch::builder()
        .declare_resource(ResourceSpecification {
            id: owner.clone(),
            role: ResourceRole::Checkpoint,
            ownership: ResourceOwnership::Borrowed,
            binding: "progress".into(),
        })?
        .provide_resource(
            owner,
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
        for name in ["source", "query", "sink"] {
            tokio::time::timeout(
                Duration::from_secs(10),
                core.computation_component(name)?.wait_started(),
            )
            .await??;
        }
        Result::<()>::Ok(())
    }
    .await;
    if let Err(error) = started {
        core.shutdown()
            .await
            .context("ordinary PostgreSQL graph cleanup")?;
        return Err(error);
    }
    Ok((core, progress, results, received))
}

async fn receive(receiver: &mut tokio::sync::mpsc::Receiver<InputEnvelope>) -> Result<Value> {
    let output = tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await?
        .context("closed row receiver")?;
    let result = QueryChangeCodec::to_legacy_result(&output.envelope)?;
    let result = result.results.first().context("missing row result")?;
    Ok(match result {
        drasi_lib::channels::ResultDiff::Add { data, .. } => data.clone(),
        drasi_lib::channels::ResultDiff::Update { after, .. } => after.clone(),
        other => anyhow::bail!("unexpected row result {other:?}"),
    })
}

#[tokio::test(flavor = "current_thread")]
async fn native_rows_support_ordinary_memory_queries_without_transaction_settings() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = libraries::plugins(dynamic)?;
        let mut database = Database::new().await?;
        database.config.connection.slot_name = "ordinary_slot".into();
        let (core, progress, results, mut received) =
            row_graph(&plugins, &database, None, false).await?;
        database
            .client()
            .batch_execute(
                "BEGIN; INSERT INTO person VALUES(1,'first'); UPDATE person SET name='middle';
             UPDATE person SET name='last'; COMMIT;",
            )
            .await?;
        for name in ["first", "middle", "last"] {
            assert_eq!(receive(&mut received).await?, json!({"id":1,"name":name}));
        }
        assert_eq!(results.snapshot()?.rows.len(), 1);
        let state = progress.snapshot();
        assert!(!state.persistent);
        assert_eq!(state.transport_sequences[&StreamId::try_new("changes")?], 3);
        let position: RowPosition = serde_json::from_slice(
            state.checkpoints[&SourceProgressKey::Source("source".into())]
                .source_position
                .as_ref()
                .context("volatile row position")?,
        )?;
        assert_eq!(position.sequence, 3);
        database
            .wait_slot_feedback("ordinary_slot", position.feedback_lsn())
            .await?;
        let retired: bool = database.client().query_one(
            "SELECT pg_terminate_backend(active_pid) FROM pg_replication_slots WHERE slot_name='ordinary_slot'", &[],
        ).await?.get(0);
        assert!(retired);
        database
            .client()
            .batch_execute("INSERT INTO person VALUES(2,'after reconnect')")
            .await?;
        assert_eq!(
            receive(&mut received).await?,
            json!({"id":2,"name":"after reconnect"})
        );
        assert_eq!(results.snapshot()?.rows.len(), 2);
        core.shutdown().await?;
        drop((core, results, received));
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_rows_bootstrap_and_resume_ordinary_persistent_queries() -> Result<()> {
    let plugins = libraries::plugins(true)?;
    let database = Database::new().await?;
    database
        .client()
        .batch_execute("INSERT INTO person VALUES(1,'snapshot')")
        .await?;
    let path = tempfile::tempdir()?;
    let (core, progress, results, mut received) =
        row_graph(&plugins, &database, Some(path.path()), true).await?;
    assert_eq!(results.snapshot()?.rows.len(), 1);
    let slot = snapshot::managed_slot(&database).await?;
    database
        .client()
        .batch_execute("BEGIN; UPDATE person SET name='one'; UPDATE person SET name='two'; COMMIT;")
        .await?;
    assert_eq!(receive(&mut received).await?, json!({"id":1,"name":"one"}));
    assert_eq!(receive(&mut received).await?, json!({"id":1,"name":"two"}));
    core.shutdown().await?;
    drop((core, progress, results, received));
    database
        .client()
        .batch_execute("UPDATE person SET name='offline'")
        .await?;
    let (core, _progress, results, mut received) =
        row_graph(&plugins, &database, Some(path.path()), true).await?;
    assert_eq!(snapshot::managed_slot(&database).await?, slot);
    assert_eq!(
        receive(&mut received).await?,
        json!({"id":1,"name":"offline"})
    );
    assert_eq!(results.snapshot()?.rows.len(), 1);
    core.shutdown().await?;
    drop((core, results, received));
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_row_restart_replays_only_the_unhandled_postgres_transaction_suffix() -> Result<()>
{
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    for restart in [false, true] {
        let progress = Arc::new(QuerySourceProgress::new(
            "row-replay",
            ComponentId::try_new("query")?,
        )?);
        let mut query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: "row-replay".into(),
                id: ComponentId::try_new("query")?,
                query: "MATCH (n:person) RETURN n.id AS id,n.name AS name".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new("results")?,
                outbox_capacity: size(16),
            },
            libraries::indexes(path.path())?,
        )
        .await?
        .with_source_progress(progress.clone())?;
        query.start().await?;
        let mut source = drasi_computation_source_postgres::PostgresTransactionSource::rows(
            ComponentId::try_new("source")?,
            StreamId::try_new("changes")?,
            database.config.clone(),
            SourceProgressReader::from(progress.clone()).read_only(),
            false,
            drasi_computation_source_postgres::RowSourceOptions {
                persistent: true,
                auto_create_slot: false,
                start_from_slot: false,
                ..Default::default()
            },
        )?;
        source.start().await?;
        if !restart {
            database
                .client()
                .batch_execute("INSERT INTO person VALUES(1,'first'),(2,'remaining')")
                .await?;
        }
        let row = tokio::time::timeout(Duration::from_secs(8), source.next())
            .await??
            .context("row")?;
        let expected = if restart { 2 } else { 1 };
        let raw = GraphChangeCodec::source_metadata(&row.envelope)?.context("source metadata")?;
        assert_eq!(raw.sequence, Some(expected));
        let outputs = query
            .transform(InputEnvelope {
                port: PortId::try_new("in")?,
                envelope: row.envelope,
            })
            .await?;
        query.delivery_completed(&outputs).await?;
        assert_eq!(rows(&query).len(), expected as usize);
        let state = progress.snapshot();
        let position: RowPosition = serde_json::from_slice(
            state.checkpoints[&SourceProgressKey::Source("source".into())]
                .source_position
                .as_ref()
                .context("cursor")?,
        )?;
        if restart {
            assert!(position.complete());
            database.wait_feedback(position.commit_lsn).await?;
        } else {
            assert!(!position.complete());
            assert_eq!(database.feedback().await?, database.config.start_lsn);
        }
        source.stop().await?;
        query.stop().await?;
    }
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_row_reconnect_keeps_queued_work_and_replays_from_the_actual_partial_cursor(
) -> Result<()> {
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    let progress = Arc::new(QuerySourceProgress::new(
        "row-reconnect",
        ComponentId::try_new("query")?,
    )?);
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "row-reconnect".into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:person) RETURN n.id AS id,n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("results")?,
            outbox_capacity: size(16),
        },
        libraries::indexes(path.path())?,
    )
    .await?
    .with_source_progress(progress.clone())?;
    query.start().await?;
    let mut source = drasi_computation_source_postgres::PostgresTransactionSource::rows(
        ComponentId::try_new("source")?,
        StreamId::try_new("changes")?,
        database.config.clone(),
        SourceProgressReader::from(progress.clone()).read_only(),
        false,
        drasi_computation_source_postgres::RowSourceOptions {
            persistent: true,
            auto_create_slot: false,
            start_from_slot: false,
            reconnect_attempts: 3,
            reconnect_delay_ms: 100,
        },
    )?;
    source.start().await?;
    database
        .client()
        .batch_execute("INSERT INTO person VALUES(1,'one'),(2,'two'),(3,'three')")
        .await?;
    let first = tokio::time::timeout(Duration::from_secs(5), source.next())
        .await??
        .context("first row")?;
    let output = query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: first.envelope,
        })
        .await?;
    query.delivery_completed(&output).await?;
    let held = tokio::time::timeout(Duration::from_secs(5), source.next())
        .await??
        .context("queued row")?;
    let old_pid: i32 = database
        .client()
        .query_one(
            "SELECT active_pid FROM pg_replication_slots WHERE slot_name='native_slot'",
            &[],
        )
        .await?
        .get(0);
    assert!(database
        .client()
        .query_one("SELECT pg_terminate_backend($1)", &[&old_pid])
        .await?
        .get::<_, bool>(0));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let pid: Option<i32> = database
                .client()
                .query_one(
                    "SELECT active_pid FROM pg_replication_slots WHERE slot_name='native_slot'",
                    &[],
                )
                .await?
                .get(0);
            if pid.is_some_and(|pid| pid != old_pid) {
                return Result::<()>::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(
        database.feedback().await?,
        database.config.start_lsn,
        "a queued suffix is not completion"
    );
    let output = query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: held.envelope,
        })
        .await?;
    query.delivery_completed(&output).await?;
    let mut replayed_second = false;
    for _ in 0..4 {
        let next = tokio::time::timeout(Duration::from_secs(5), source.next())
            .await??
            .context("reconnected row")?;
        let logical = GraphChangeCodec::source_metadata(&next.envelope)?
            .context("row metadata")?
            .sequence
            .context("logical sequence")?;
        let output = query
            .transform(InputEnvelope {
                port: PortId::try_new("in")?,
                envelope: next.envelope,
            })
            .await?;
        if logical == 2 {
            replayed_second = true;
            assert!(
                output.is_empty(),
                "queued duplicate must not repeat the query effect"
            );
        }
        query.delivery_completed(&output).await?;
        if replayed_second
            && progress.snapshot().checkpoints[&SourceProgressKey::Source("source".into())].sequence
                == 3
        {
            break;
        }
    }
    assert!(replayed_second);
    assert_eq!(rows(&query).len(), 3);
    let position: RowPosition = serde_json::from_slice(
        progress.snapshot().checkpoints[&SourceProgressKey::Source("source".into())]
            .source_position
            .as_ref()
            .context("complete cursor")?,
    )?;
    assert!(position.complete());
    database.wait_feedback(position.feedback_lsn()).await?;
    source.stop().await?;
    query.stop().await?;
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn standalone_snapshot_needs_only_select_privileges_and_no_wal_or_slot() -> Result<()> {
    let container = GenericImage::new("postgres", "16-alpine")
        .with_exposed_port(ContainerPort::Tcp(5432))
        .with_wait_for(WaitFor::log(
            LogWaitStrategy::stdout_or_stderr("database system is ready to accept connections")
                .with_times(2),
        ))
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .start()
        .await?;
    let mut config = tokio_postgres::Config::new();
    config
        .host(container.get_host().await?.to_string())
        .port(container.get_host_port_ipv4(5432).await?)
        .dbname("postgres")
        .user("postgres")
        .password("postgres");
    let (client, connection) = config.connect(NoTls).await?;
    let mut driver = Some(tokio::spawn(connection));
    client.batch_execute(
        "CREATE DOMAIN short_label AS text;
         CREATE TABLE initial(id INTEGER PRIMARY KEY,label short_label,optional TEXT,items INTEGER[],data JSONB);
         INSERT INTO initial VALUES(1,'007',NULL,ARRAY[1,NULL,3],'{\"ready\":true}');
         CREATE TABLE keyless(name TEXT); INSERT INTO keyless VALUES('same'),('same');
         CREATE ROLE snapshot_reader LOGIN PASSWORD 'snapshot' NOREPLICATION;
         GRANT SELECT ON initial,keyless TO snapshot_reader;"
    ).await?;
    let wal: String = client.query_one("SHOW wal_level", &[]).await?.get(0);
    assert_eq!(wal, "replica");
    for dynamic in [false, true] {
        let plugins = libraries::plugins(dynamic)?;
        let factory = plugins
            .bootstrap
            .bootstrap_factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == drasi_computation_bootstrap_postgres::POSTGRES_SNAPSHOT
            })
            .context("standalone snapshot factory")?;
        let mut settings = json!({
                "host":container.get_host().await?.to_string(),"port":container.get_host_port_ipv4(5432).await?,
                "database":"postgres","user":"snapshot_reader","password":"snapshot",
                "tables":["initial","keyless"],"max_tables":8,"max_protocol_bytes":65536,
                "io_timeout_ms":5000,"max_duration_ms":10000,"ssl_mode":"disable"
        });
        let create = |settings: Value, graph: &str| {
            factory.create_provider(
                ComponentId::try_new("query")?,
                json!({"source":"external","stream":"external/out","settings":settings}),
                Some(libraries::scope(graph)),
                None,
            )
        };
        let provider = create(settings.clone(), "standalone")?;
        provider.prepare().await?;
        let snapshot = provider.snapshot().await?;
        assert!(snapshot.watermarks.is_empty());
        let envelopes = snapshot.changes.try_collect::<Vec<_>>().await?;
        assert_eq!(envelopes.len(), 3);
        let changes = envelopes
            .iter()
            .map(GraphChangeCodec::decode_changes)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        assert_ne!(
            changes[1].get_reference(),
            changes[2].get_reference(),
            "keyless rows must not collapse"
        );
        let serialized = serde_json::to_value(&changes[0])?;
        let text = serialized.to_string();
        assert!(text.contains("007") && text.contains("ready"), "{text}");
        assert!(provider.complete_snapshot().await?.is_empty());
        assert!(provider.completion_state()?.is_none());
        provider.stop().await?;

        settings["tables"] = json!(["initial"]);
        let provider = create(settings.clone(), ComponentBatch::GRAPH_ID)?;
        let query=ContinuousQueryTransformer::new(ContinuousQueryDefinition {
            graph_id:ComponentBatch::GRAPH_ID.into(),id:ComponentId::try_new("query")?,
            query:"MATCH (n:initial) RETURN n.id AS id,n.label AS label,n.optional AS optional,n.items AS items,n.data AS data".into(),
            language:ComputationQueryLanguage::Cypher,output_stream:StreamId::try_new("results")?,outbox_capacity:size(8),
        },Arc::new(InMemoryComputationProvider)).await?.with_recovery_scope(INSTANCE)?.with_bootstrap(provider);
        let results = query.results();
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let ingress = Ingress {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("external")?,
                vec![PortDescriptor::new(
                    PortId::try_new("out")?,
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?,
            receiver,
        };
        let (sink, mut received) = drasi_reaction_application::NativeApplicationReaction::channel(
            "sink",
            QueryChangeCodec::schema().descriptor().clone(),
            size(1),
        )?;
        let batch = ComponentBatch::builder()
            .source(Box::new(ingress))
            .query(Box::new(query))
            .sink(Box::new(sink))
            .bind_stream(
                endpoint("external", "out"),
                StreamId::try_new("external/out")?,
            )
            .bind_stream(endpoint("query", "out"), StreamId::try_new("results")?)
            .connect(
                EdgeDefinition::new(endpoint("external", "out"), endpoint("query", "in")),
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
        core.start().await?;
        tokio::time::timeout(
            Duration::from_secs(10),
            core.computation_component("query")?.wait_started(),
        )
        .await??;
        let snapshot = results.snapshot()?;
        let row = QueryChangeCodec::decode_row(
            snapshot.rows.values().next().context("initial query row")?,
        )?;
        assert_eq!(
            QueryChangeCodec::row_values_to_json(&row.values),
            json!({"id":1,"label":"007","optional":null,"items":[1,null,3],"data":"{\"ready\":true}"})
        );
        let SourceChange::Insert {
            element:
                Element::Node {
                    mut metadata,
                    mut properties,
                },
        } = changes[0].clone()
        else {
            anyhow::bail!("expected initial node");
        };
        let now: i64 = client
            .query_one(
                "SELECT floor(extract(epoch FROM clock_timestamp())*1000)::bigint",
                &[],
            )
            .await?
            .get(0);
        metadata.effective_from = u64::try_from(now)?;
        properties.insert("label", ElementValue::String("external update".into()));
        sender
            .send(OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: GraphChangeCodec::encode_change(
                    SourceChange::Update {
                        element: Element::Node {
                            metadata,
                            properties,
                        },
                    },
                    StreamId::try_new("external/out")?,
                    1,
                    None,
                )?,
            })
            .await?;
        assert_eq!(
            receive(&mut received).await?["label"],
            "external update",
            "a standalone PostgreSQL snapshot must not invent a WAL watermark on another source"
        );
        core.shutdown().await?;
        drop(core);
        let provider = create(settings.clone(), "standalone")?;
        provider.prepare().await?;
        let mut old = provider.snapshot().await?.changes;
        old.next().await.context("initial old snapshot row")??;
        provider.stop().await?;
        provider.prepare().await?;
        let replacement = provider.snapshot().await?.changes;
        assert!(old.next().await.context("stale snapshot error")?.is_err());
        drop(old);
        assert_eq!(replacement.try_collect::<Vec<_>>().await?.len(), 1);
        assert!(provider.complete_snapshot().await?.is_empty());
        provider.stop().await?;
        provider.prepare().await?;
        drop(provider.snapshot().await?);
        assert!(
            provider.snapshot().await.is_err(),
            "dropped streams require cleanup"
        );
        provider.stop().await?;

        let mut missing = settings.clone();
        missing["tables"] = json!(["missing_table"]);
        let rejected = create(missing, "standalone")?;
        rejected.prepare().await?;
        assert!(
            rejected.snapshot().await.is_err(),
            "missing tables must not become empty success"
        );
        rejected.stop().await?;

        if dynamic {
            client
                .execute(
                    "INSERT INTO initial(id,label) VALUES(2,$1::text)",
                    &[&"large".repeat(2000)],
                )
                .await?;
            settings["max_protocol_bytes"] = json!(2048);
            let bounded = create(settings, "standalone")?;
            bounded.prepare().await?;
            let snapshot = bounded.snapshot().await?;
            assert!(snapshot.changes.try_collect::<Vec<_>>().await.is_err());
            assert!(bounded.complete_snapshot().await.is_err());
            bounded.stop().await?;
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let sessions: i64 = client
                    .query_one(
                        "SELECT count(*) FROM pg_stat_activity WHERE usename='snapshot_reader'",
                        &[],
                    )
                    .await?
                    .get(0);
                if sessions == 0 {
                    return Result::<()>::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
    }
    let slots: i64 = client
        .query_one("SELECT count(*) FROM pg_replication_slots", &[])
        .await?
        .get(0);
    assert_eq!(slots, 0);
    drop(client);
    match join_owned_worker(&mut driver, Duration::from_secs(5)).await? {
        WorkerCompletion::Completed(result) => result?,
        other => anyhow::bail!("admin driver did not complete: {other:?}"),
    }
    container.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn temporary_exported_snapshot_preserves_configured_slot_and_imported_readers() -> Result<()>
{
    use drasi_postgres_common::replication::{connect, field};
    let database = Database::new().await?;
    database
        .client()
        .batch_execute("INSERT INTO person VALUES(1,'before snapshot')")
        .await?;
    let original = database.feedback().await?;
    let mut exporter = connect(&database.config).await?;
    let temporary = format!("snapshot_{}", uuid::Uuid::new_v4().simple());
    let rows = exporter
        .query_rows(
            format!(
                "CREATE_REPLICATION_SLOT {temporary} TEMPORARY LOGICAL pgoutput EXPORT_SNAPSHOT"
            ),
            database.config.max_protocol_bytes,
        )
        .await?;
    let [slot] = rows.as_slice() else {
        anyhow::bail!("missing exported snapshot");
    };
    assert_eq!(field(slot, 0)?, temporary);
    let boundary = field(slot, 1)?;
    let snapshot_name = field(slot, 2)?;
    assert!(snapshot_name
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'));
    let mut first = connect(&database.config).await?;
    let mut second = connect(&database.config).await?;
    for reader in [&mut first, &mut second] {
        reader.query_rows(
            format!("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET TRANSACTION SNAPSHOT '{snapshot_name}'"),
            database.config.max_protocol_bytes,
        ).await?;
    }
    database.client().batch_execute(
        "BEGIN; UPDATE person SET name='after snapshot' WHERE id=1; INSERT INTO person VALUES(2,'live'); COMMIT;"
    ).await?;
    drop(exporter);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let present: bool = database
                .client()
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM pg_replication_slots WHERE slot_name=$1)",
                    &[&temporary],
                )
                .await?
                .get(0);
            if !present {
                return Result::<()>::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    for reader in [&mut first, &mut second] {
        let rows = reader
            .query_rows(
                "SELECT id::text,name FROM person ORDER BY id".into(),
                database.config.max_protocol_bytes,
            )
            .await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(field(&rows[0], 0)?, "1");
        assert_eq!(field(&rows[0], 1)?, "before snapshot");
        reader
            .query_rows("COMMIT".into(), database.config.max_protocol_bytes)
            .await?;
    }
    drop((first, second));
    assert_eq!(
        database.feedback().await?,
        original,
        "temporary snapshots must not acknowledge the configured slot"
    );
    let path = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(path.path(), progress.clone()).await?;
    query.start().await?;
    let mut settings = database.config.clone();
    settings.start_lsn = boundary;
    let mut source = super::source(settings, progress)?;
    source.start().await?;
    let next = super::next(&mut source).await?;
    let changes = SourceTransactionCodec::decode(&next.envelope, limits(16))?.into_changes();
    assert_eq!(
        changes.len(),
        2,
        "configured slot must retain exactly the post-snapshot transaction"
    );
    source.stop().await?;
    query.stop().await?;
    database.shutdown().await
}
