// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use anyhow::{Context, Result};
use futures_util::StreamExt;

struct Graph {
    core: DrasiLib,
    owners: Vec<Arc<QuerySourceProgress>>,
    rows: Vec<QueryResults>,
    outputs: Vec<mpsc::Receiver<ChangeEnvelope>>,
}

async fn graph(
    database: &Database,
    plugins: &[Arc<NativePlugin>],
    source_settings: Value,
    bootstrap_settings: Option<(Value, Vec<String>)>,
    query_text: &str,
) -> Result<Graph> {
    graph_mode(
        database,
        plugins,
        source_settings,
        bootstrap_settings,
        query_text,
        false,
    )
    .await
}

async fn graph_mode(
    database: &Database,
    plugins: &[Arc<NativePlugin>],
    source_settings: Value,
    bootstrap_settings: Option<(Value, Vec<String>)>,
    query_text: &str,
    positional: bool,
) -> Result<Graph> {
    let tracked = !source_settings["replay"].is_null();
    let source_name = match (tracked, positional) {
        (true, false) => drasi_computation_source_mysql::MYSQL_SHARED_CHANGES,
        (false, false) => drasi_computation_source_mysql::MYSQL_CHANGES,
        (true, true) => drasi_computation_source_mysql::MYSQL_SHARED_POSITIONAL_CHANGES,
        (false, true) => drasi_computation_source_mysql::MYSQL_POSITIONAL_CHANGES,
    };
    let source_factory = plugins[0]
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == source_name)
        .context("native ordinary-row factory")?
        .clone();
    let sink_factory = plugins[2]
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == drasi_computation_sink_storedproc_mysql::MYSQL_STOREDPROC
        })
        .context("native procedure factory")?
        .clone();
    let mut source =
        source_factory.specification(id("source"), json!({"stream":"changes","settings":{}}))?;
    let mut sink = sink_factory.specification(id("procedures"), json!({"settings":{}}))?;
    let reference = |key: &str| ConfigurationValue::Reference {
        resource: resource("settings"),
        key: key.into(),
        secret: true,
    };
    source
        .configuration
        .insert("settings".into(), reference("source"));
    let owners = ["left", "right"]
        .iter()
        .map(|name| {
            Ok(Arc::new(QuerySourceProgress::new(
                ComponentBatch::GRAPH_ID,
                id(name),
            )?))
        })
        .collect::<Result<Vec<_>>>()?;
    if tracked {
        source
            .dependencies
            .insert("consumer_progress".into(), vec![resource("consumers")]);
    }
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
                    source: source_settings,
                    sink: database.sink_settings(),
                }))),
            ),
        )?
        .component(source, source_factory)
        .component(sink, sink_factory)
        .bind_stream(endpoint("source", "out"), stream("changes"));
    if tracked {
        builder = builder
            .declare_resource(ResourceSpecification {
                id: resource("consumers"),
                role: ResourceRole::Checkpoint,
                ownership: ResourceOwnership::Borrowed,
                binding: "consumers".into(),
            })?
            .provide_resource(
                resource("consumers"),
                ResourceHandle::new(
                    ResourceRole::Checkpoint,
                    Arc::new(ConsumerProgressSet::new(
                        ComponentBatch::GRAPH_ID,
                        id("source"),
                        owners.iter().cloned(),
                    )?),
                ),
            )?;
    }
    let mut rows = Vec::new();
    let mut outputs = Vec::new();
    for owner in &owners {
        let name = owner.component_id().as_str();
        let mut definition = query_definition(name);
        definition.query = query_text.into();
        let mut query = ContinuousQueryTransformer::new_configured(
            definition,
            Arc::new(InMemoryComputationProvider),
            QueryOptions::default(),
            QueryExecutionSettings::default(),
            None,
        )
        .await?
        .with_source_progress(owner.clone())?
        .with_recovery_scope(INSTANCE)?;
        if let Some((settings, labels)) = &bootstrap_settings {
            let factory = plugins[1]
                .bootstrap_factories()
                .iter()
                .find(|factory| {
                    factory.metadata().implementation.name.as_ref()
                        == if tracked {
                            drasi_computation_bootstrap_mysql::MYSQL_SHARED_ROW_BOOTSTRAP
                        } else {
                            drasi_computation_bootstrap_mysql::MYSQL_STANDALONE_BOOTSTRAP
                        }
                })
                .context("native standalone snapshot")?;
            let mut configuration =
                json!({"source":"source","stream":"changes","settings":settings});
            if !labels.is_empty() {
                anyhow::ensure!(
                    !tracked,
                    "requested labels belong to the standalone factory"
                );
                configuration["labels"] = json!(labels);
            }
            query = query.with_bootstrap(factory.create_provider(
                id(name),
                configuration,
                Some(scope()),
                tracked.then(|| owner.clone()),
            )?);
        }
        rows.push(query.results());
        let (sender, receiver) = mpsc::channel(32);
        outputs.push(receiver);
        let capture = format!("{name}-capture");
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
    Ok(Graph {
        core,
        owners,
        rows,
        outputs,
    })
}

async fn finish(graph: Graph, database: &mut Database, proof: Result<()>) -> Result<()> {
    let stopped = graph.core.shutdown().await.map_err(anyhow::Error::from);
    let sessions = database.expect_count(
        "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER IN ('root','snapshot_reader') AND ID<>CONNECTION_ID()","0",
    ).await;
    let removed = database.cleanup();
    combine(
        "native MySQL legacy capability proof",
        [proof, stopped, sessions, removed],
    )
}

async fn positional_live(dynamic: bool, gtid: bool) -> Result<()> {
    let mut database = Database::start(gtid, true).await?;
    database
        .initialize_columns(
            "id INT PRIMARY KEY,value VARCHAR(80) NOT NULL,wide BIGINT UNSIGNED,raw_value VARBINARY(8)",
        )
        .await?;
    let plugins = libraries(dynamic)?;
    let mut settings = database.source_settings();
    settings["replay"] = Value::Null;
    settings["start"] = if gtid {
        json!({"mode":"gtid","set":database.sql("SELECT @@global.gtid_executed").await?})
    } else {
        json!({"mode":"end"})
    };
    settings["table_keys"] = json!([{"table":"items","key_columns":["@0"]}]);
    let mut graph = graph_mode(
        &database,
        &plugins,
        settings,
        None,
        "MATCH (n:items) RETURN n.`@0` AS id,n.`@1` AS value,n.`@2` AS wide,n.`@3` AS bytes",
        true,
    )
    .await?;
    let proof = async {
        tokio::time::timeout(Duration::from_secs(30), graph.core.start()).await??;
        let schema = graph
            .core
            .get_source_schema("source")
            .await?
            .context("positional catalog schema")?;
        assert_eq!(
            schema.nodes[0]
                .properties
                .iter()
                .map(|property| property.name.as_str())
                .collect::<Vec<_>>(),
            ["@0", "@1", "@2", "@3"],
        );
        database
            .sql(
                "SET NAMES utf8mb4; INSERT INTO items VALUES \
            (2,CONVERT(0x636166C3A9 USING utf8mb4),18446744073709551615,0x00FF)",
            )
            .await?;
        for output in &mut graph.outputs {
            next(output).await?;
        }
        for result in &graph.rows {
            assert_eq!(
                rows(result)?,
                [json!({
                    "id":2,"value":"caf\u{e9}","wide":"18446744073709551615","bytes":[0,255]
                })]
            );
        }
        database
            .expect_count("SELECT COUNT(*) FROM audit", "2")
            .await?;
        database
            .sql("UPDATE items SET id=20,value='rekey' WHERE id=2")
            .await?;
        for output in &mut graph.outputs {
            next(output).await?;
            next(output).await?;
        }
        for result in &graph.rows {
            assert_eq!(
                rows(result)?,
                [json!({
                    "id":20,"value":"rekey","wide":"18446744073709551615","bytes":[0,255]
                })]
            );
        }
        database
            .expect_count("SELECT COUNT(*) FROM audit", "6")
            .await?;
        database.sql("DELETE FROM items WHERE id=20").await?;
        for output in &mut graph.outputs {
            next(output).await?;
        }
        for result in &graph.rows {
            assert!(rows(result)?.is_empty());
        }
        database
            .expect_count("SELECT COUNT(*) FROM audit", "8")
            .await?;
        Ok(())
    }
    .await;
    finish(graph, &mut database, proof).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn positional_live_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        for gtid in [false, true] {
            positional_live(dynamic, gtid).await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn positional_live_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        for gtid in [false, true] {
            positional_live(dynamic, gtid).await?;
        }
    }
    Ok(())
}

async fn positional_historical_catalog_fence(dynamic: bool) -> Result<()> {
    let mut database = Database::start(false, true).await?;
    database
        .initialize_columns(
            "id BIGINT UNSIGNED PRIMARY KEY,value VARCHAR(80) CHARACTER SET latin1 NOT NULL",
        )
        .await?;
    let status = database.sql("SHOW MASTER STATUS").await?;
    let mut fields = status.split('\t');
    let file = fields.next().context("historical binlog file")?;
    let offset: u32 = fields
        .next()
        .context("historical binlog position")?
        .parse()?;
    database
        .sql(
            "INSERT INTO items VALUES(18446744073709551615,CONVERT(0xC3A9 USING latin1)); \
        DELETE FROM items WHERE id=18446744073709551615; \
        USE mysql; ALTER TABLE test.items MODIFY id BIGINT NOT NULL, \
        CHANGE value renamed VARCHAR(80) CHARACTER SET utf8mb4 NOT NULL",
        )
        .await?;
    let plugins = libraries(dynamic)?;
    let mut settings = database.source_settings();
    settings["replay"] = Value::Null;
    settings["start"] = json!({"mode":"position","file":file,"position":offset});
    settings["table_keys"] = json!([{"table":"items","key_columns":["@0"]}]);
    let mut rejected = graph_mode(
        &database,
        &plugins,
        settings.clone(),
        None,
        "MATCH (n:items) RETURN n.`@0` AS id,n.`@1` AS value",
        true,
    )
    .await?;
    let proof = async {
        let started = tokio::time::timeout(Duration::from_secs(30), rejected.core.start()).await?;
        let error = started.expect_err("historical catalog mismatch must fence before rows");
        anyhow::ensure!(
            format!("{error:#}").contains("catalog-changing"),
            "{error:#}"
        );
        for result in &rejected.rows {
            anyhow::ensure!(
                result.snapshot()?.as_of_sequence == 0 && rows(result)?.is_empty(),
                "historical rows were applied using the wrong current catalog"
            );
        }
        for output in &mut rejected.outputs {
            anyhow::ensure!(
                output.try_recv().is_err(),
                "incorrect historical output escaped"
            );
        }
        anyhow::ensure!(
            database.sql("SELECT COUNT(*) FROM audit").await? == "0",
            "wrongly decoded history reached procedures"
        );
        Ok(())
    }
    .await;
    let cleanup = rejected.core.shutdown().await.map_err(anyhow::Error::from);
    drop(rejected);
    combine(
        "positional historical charset/signedness/name fence",
        [proof, cleanup],
    )?;
    settings["start"] = json!({"mode":"end"});
    let mut matching = graph_mode(
        &database,
        &plugins,
        settings,
        None,
        "MATCH (n:items) RETURN n.`@0` AS id,n.`@1` AS value",
        true,
    )
    .await?;
    let proof = async {
        matching.core.start().await?;
        database
            .sql("INSERT INTO items(id,renamed) VALUES(2,'matching-profile')")
            .await?;
        for output in &mut matching.outputs {
            next(output).await?;
        }
        for result in &matching.rows {
            assert_eq!(rows(result)?, [json!({"id":2,"value":"matching-profile"})]);
        }
        Ok(())
    }
    .await;
    finish(matching, &mut database, proof).await
}

async fn positional_checkpoint_catalog_fence(dynamic: bool) -> Result<()> {
    let mut database = Database::start(false, true).await?;
    database.initialize().await?;
    let plugins = libraries(dynamic)?;
    let path = tempfile::tempdir()?;
    let mut graph = tracked_graph(
        &database,
        &plugins,
        path.path(),
        TrackedMode::PositionalRows,
        Arc::new(Semaphore::new(64)),
    )
    .await?;
    let key = SourceProgressKey::Source("source".into());
    let mut checkpoints = Vec::new();
    let proof = async {
        graph.core.start().await?;
        for output in &mut graph.outputs {
            next(output).await?;
        }
        for owner in &graph.owners {
            checkpoints.push(position(owner, &key)?);
        }
        Ok(())
    }
    .await;
    let cleanup = graph.core.shutdown().await.map_err(anyhow::Error::from);
    drop(graph);
    combine(
        "positional checkpoint before catalog change",
        [proof, cleanup],
    )?;
    database
        .sql(
            "ALTER TABLE items MODIFY id INT UNSIGNED NOT NULL, \
        CHANGE value renamed VARCHAR(80) CHARACTER SET utf8mb4 NOT NULL",
        )
        .await?;
    let recovered = tracked_graph(
        &database,
        &plugins,
        path.path(),
        TrackedMode::PositionalRows,
        Arc::new(Semaphore::new(64)),
    )
    .await?;
    let proof = async {
        let error = tokio::time::timeout(Duration::from_secs(30), recovered.core.start())
            .await?
            .expect_err("changed catalog must not reinterpret a checkpoint");
        anyhow::ensure!(format!("{error:#}").contains("binding"), "{error:#}");
        for (owner, expected) in recovered.owners.iter().zip(&checkpoints) {
            assert_eq!(&position(owner, &key)?, expected);
            assert_eq!(owner.snapshot().reset_generation, 0);
        }
        Ok(())
    }
    .await;
    recovered.gate.close();
    let cleanup = recovered.core.shutdown().await.map_err(anyhow::Error::from);
    drop(recovered);
    let removed = database.cleanup();
    combine(
        "positional checkpoint catalog fence",
        [proof, cleanup, removed],
    )
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn positional_catalog_fences_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        positional_historical_catalog_fence(dynamic).await?;
        positional_checkpoint_catalog_fence(dynamic).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn positional_catalog_fences_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        positional_historical_catalog_fence(dynamic).await?;
        positional_checkpoint_catalog_fence(dynamic).await?;
    }
    Ok(())
}

async fn legacy_sql_roles(dynamic: bool) -> Result<()> {
    let mut legacy = Database::start(false, true).await?;
    let mut replication = Database::new().await?;
    legacy.initialize().await?;
    legacy
        .sql("SET NAMES utf8mb4; UPDATE items SET value=CONVERT(0x636166C3A9 USING utf8mb4) WHERE id=1")
        .await?;
    legacy
        .sql(
            "CREATE USER snapshot_reader@'%' IDENTIFIED BY 'snapshot-only'; \
        GRANT SELECT ON test.items TO snapshot_reader@'%'",
        )
        .await?;
    replication.initialize().await?;
    replication
        .sql("UPDATE items SET value='not-the-snapshot' WHERE id=1")
        .await?;
    let plugins = libraries(dynamic)?;
    let standalone = |mut settings: Value| {
        settings["replay"] = Value::Null;
        settings["start"] = json!({"mode":"end"});
        settings
    };
    let mut snapshot = standalone(legacy.source_settings());
    snapshot["connection"]["user"] = json!("snapshot_reader");
    snapshot["connection"]["password"] = json!("snapshot-only");
    let mut graph = graph(
        &legacy,
        &plugins,
        standalone(replication.source_settings()),
        Some((snapshot, Vec::new())),
        "MATCH (n:items) RETURN n.id AS id,n.value AS value",
    )
    .await?;
    let proof = async {
        assert!(legacy.sql("SELECT VERSION()").await?.starts_with("5.7."));
        tokio::time::timeout(Duration::from_secs(30), graph.core.start()).await??;
        for result in &graph.rows {
            assert_eq!(rows(result)?, [json!({"id":1,"value":"caf\u{e9}"})]);
        }
        for owner in &graph.owners {
            assert!(owner.snapshot().checkpoints.is_empty());
        }
        replication
            .sql(
                "BEGIN; UPDATE items SET value='live' WHERE id=1; \
                INSERT INTO items VALUES(2,'second'); COMMIT;",
            )
            .await?;
        for output in &mut graph.outputs {
            next(output).await?;
            next(output).await?;
        }
        legacy
            .expect_count("SELECT COUNT(*) FROM audit", "4")
            .await?;
        assert_eq!(
            legacy
                .sql("SELECT query_id,id,value FROM effects ORDER BY query_id,id")
                .await?,
            "left\t1\tlive\nleft\t2\tsecond\nright\t1\tlive\nright\t2\tsecond"
        );
        replication.sql("DELETE FROM items WHERE id=2").await?;
        for output in &mut graph.outputs {
            next(output).await?;
        }
        legacy
            .expect_count("SELECT COUNT(*) FROM audit", "6")
            .await?;
        assert_eq!(
            legacy
                .sql("SELECT COUNT(*) FROM effects WHERE id=2")
                .await?,
            "0"
        );
        Ok(())
    }
    .await;
    let graph_cleanup = finish(graph, &mut legacy, proof).await;
    let sessions = replication.expect_count(
        "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='root' AND ID<>CONNECTION_ID()",
        "0",
    ).await;
    let removed = replication.cleanup();
    combine(
        "MySQL 5.7 SQL roles with modern CDC",
        [graph_cleanup, sessions, removed],
    )
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn legacy_sql_roles_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        legacy_sql_roles(dynamic).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn legacy_sql_roles_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        legacy_sql_roles(dynamic).await?;
    }
    Ok(())
}

async fn configured_start(dynamic: bool, gtid: bool) -> Result<()> {
    let mut database = Database::with_gtid(gtid).await?;
    database.initialize().await?;
    let plugins = libraries(dynamic)?;
    let mut settings = database.source_settings();
    settings["replay"] = Value::Null;
    settings["start"] = if gtid {
        json!({"mode":"gtid","set":database.sql("SELECT @@global.gtid_executed").await?})
    } else {
        json!({"mode":"start"})
    };
    database
        .sql("INSERT INTO items VALUES(2,'replayed')")
        .await?;
    let mut graph = graph(
        &database,
        &plugins,
        settings,
        None,
        "MATCH (n:items) RETURN n.id AS id,n.value AS value",
    )
    .await?;
    let proof = async {
        tokio::time::timeout(Duration::from_secs(30), graph.core.start()).await??;
        for output in &mut graph.outputs {
            for _ in 0..if gtid { 1 } else { 2 } {
                next(output).await?;
            }
        }
        for result in &graph.rows {
            let mut actual = rows(result)?;
            actual.sort_by_key(Value::to_string);
            let expected = if gtid {
                vec![json!({"id":2,"value":"replayed"})]
            } else {
                vec![
                    json!({"id":1,"value":"initial"}),
                    json!({"id":2,"value":"replayed"}),
                ]
            };
            assert_eq!(actual, expected);
        }
        database
            .expect_count("SELECT COUNT(*) FROM audit", if gtid { "2" } else { "4" })
            .await?;
        database
            .sql("UPDATE items SET value='live' WHERE id=2")
            .await?;
        for output in &mut graph.outputs {
            next(output).await?;
        }
        database
            .expect_count("SELECT COUNT(*) FROM audit", if gtid { "4" } else { "6" })
            .await?;
        assert_eq!(
            database
                .sql("SELECT COUNT(*) FROM effects WHERE id=2 AND value='live'")
                .await?,
            "2"
        );
        Ok(())
    }
    .await;
    finish(graph, &mut database, proof).await
}

async fn manual_keys(dynamic: bool, coordinated: bool) -> Result<()> {
    let mut database = Database::new().await?;
    database.initialize().await?;
    database.sql("ALTER TABLE items DROP PRIMARY KEY; ALTER TABLE items CHANGE id code INT NOT NULL; \
        ALTER TABLE items ADD tenant INT NOT NULL DEFAULT 1; INSERT INTO items(code,value,tenant) VALUES(2,'second',1); \
        CREATE USER snapshot_reader@'%' IDENTIFIED BY 'snapshot-only'; \
        GRANT SELECT ON test.items TO snapshot_reader@'%';").await?;
    let plugins = libraries(dynamic)?;
    let mut settings = database.source_settings();
    settings["replay"] = Value::Null;
    settings["start"] = json!({"mode":"end"});
    settings["table_keys"] = json!([{"table":"items","key_columns":["tenant","code"]}]);
    if coordinated {
        settings["replay"] = json!("server");
        settings["start"] = json!({"mode":"snapshot","lock_timeout_ms":5000});
    }
    let mut snapshot = settings.clone();
    if !coordinated {
        snapshot["connection"]["user"] = json!("snapshot_reader");
        snapshot["connection"]["password"] = json!("snapshot-only");
    }
    let mut graph = graph(
        &database,
        &plugins,
        settings,
        Some((snapshot, Vec::new())),
        "MATCH (n:items) RETURN n.code AS id,n.tenant AS tenant,n.value AS value",
    )
    .await?;
    let proof = async {
        tokio::time::timeout(Duration::from_secs(30), graph.core.start()).await??;
        for result in &graph.rows {
            let mut actual = rows(result)?;
            actual.sort_by_key(Value::to_string);
            assert_eq!(
                actual,
                [
                    json!({"id":1,"tenant":1,"value":"initial"}),
                    json!({"id":2,"tenant":1,"value":"second"})
                ]
            );
        }
        for owner in &graph.owners {
            assert!(owner.snapshot().bootstrap_complete);
            assert_eq!(
                owner.snapshot().checkpoints.is_empty(),
                !coordinated,
                "only coordinated initialization publishes its actual replay boundary"
            );
        }
        let schema = graph
            .core
            .get_source_schema("source")
            .await?
            .context("captured catalog")?;
        assert_eq!(schema.nodes[0].properties.len(), 3);
        database
            .sql("UPDATE items SET code=3,value='changed-key' WHERE code=2")
            .await?;
        for output in &mut graph.outputs {
            next(output).await?;
            next(output).await?;
        }
        for result in &graph.rows {
            let actual = rows(result)?;
            assert_eq!(actual.len(), 2);
            assert!(!actual.iter().any(|row| row["id"] == 2));
            assert!(actual.contains(&json!({"id":3,"tenant":1,"value":"changed-key"})));
        }
        database
            .expect_count("SELECT COUNT(*) FROM audit", "4")
            .await?;
        assert_eq!(
            database
                .sql("SELECT COUNT(*) FROM effects WHERE id=3 AND value='changed-key'")
                .await?,
            "2"
        );
        graph.core.stop().await?;
        database
            .sql("UPDATE items SET value='offline' WHERE code=3")
            .await?;
        tokio::time::timeout(Duration::from_secs(30), graph.core.start()).await??;
        for result in &graph.rows {
            anyhow::ensure!(
                rows(result)?.contains(&json!({"id":3,"tenant":1,"value":"offline"})),
                "memory restart did not refresh the standalone/coordinated snapshot"
            );
        }
        Ok(())
    }
    .await;
    finish(graph, &mut database, proof).await
}

async fn database_wide(dynamic: bool) -> Result<()> {
    let mut database = Database::new().await?;
    database.initialize().await?;
    let plugins = libraries(dynamic)?;
    let mut settings = database.source_settings();
    settings["replay"] = Value::Null;
    settings["start"] = json!({"mode":"end"});
    settings["tables"] = json!([]);
    settings["table_keys"] = json!([
        {"table":"audit","key_columns":["query_id","operation","id"]},
        {"table":"fresh","key_columns":["code"]}
    ]);
    let mut graph = graph(
        &database,
        &plugins,
        settings,
        None,
        "MATCH (n:fresh) RETURN n.code AS id,n.value AS value",
    )
    .await?;
    let proof = async {
        tokio::time::timeout(Duration::from_secs(30), graph.core.start()).await??;
        let before = graph
            .core
            .get_source_schema("source")
            .await?
            .context("startup catalog")?;
        assert!(!before.nodes.iter().any(|node| node.label == "fresh"));
        database
            .sql(
                "CREATE TABLE fresh(code INT NOT NULL,value VARCHAR(80)) ENGINE=MEMORY; \
            BEGIN; INSERT INTO fresh VALUES(10,'nontransactional'); ROLLBACK",
            )
            .await?;
        for output in &mut graph.outputs {
            next(output).await?;
        }
        for result in &graph.rows {
            assert_eq!(rows(result)?, [json!({"id":10,"value":"nontransactional"})]);
        }
        database
            .expect_count("SELECT COUNT(*) FROM audit", "2")
            .await?;
        let schema = graph
            .core
            .get_source_schema("source")
            .await?
            .context("live catalog")?;
        let node = schema
            .nodes
            .iter()
            .find(|node| node.label == "fresh")
            .context("new table schema")?;
        assert_eq!(
            node.properties
                .iter()
                .map(|property| property.name.as_str())
                .collect::<Vec<_>>(),
            ["code", "value"]
        );
        database
            .sql("UPDATE fresh SET value='updated' WHERE code=10")
            .await?;
        for output in &mut graph.outputs {
            next(output).await?;
        }
        database
            .expect_count("SELECT COUNT(*) FROM audit", "4")
            .await?;
        for result in &graph.rows {
            assert_eq!(rows(result)?, [json!({"id":10,"value":"updated"})]);
        }
        Ok(())
    }
    .await;
    finish(graph, &mut database, proof).await
}

async fn readonly_view(dynamic: bool) -> Result<()> {
    let mut database = Database::new().await?;
    database.initialize().await?;
    database
        .sql(
            "CREATE VIEW items_view AS SELECT id,value FROM items; \
        CREATE USER snapshot_reader@'%' IDENTIFIED BY 'snapshot-only'; \
        GRANT SELECT ON test.items_view TO snapshot_reader@'%';",
        )
        .await?;
    let plugins = libraries(dynamic)?;
    let mut settings = database.source_settings();
    settings["replay"] = Value::Null;
    settings["start"] = json!({"mode":"end"});
    let mut snapshot = settings.clone();
    snapshot["tables"] = json!(["items_view", "missing_allowed"]);
    snapshot["connection"]["user"] = json!("snapshot_reader");
    snapshot["connection"]["password"] = json!("snapshot-only");
    let graph = graph(
        &database,
        &plugins,
        settings,
        Some((
            snapshot,
            vec!["items_view".into(), "outside_allowlist".into()],
        )),
        "MATCH (n:items_view) RETURN n.id AS id,n.value AS value",
    )
    .await?;
    let proof = async {
        tokio::time::timeout(Duration::from_secs(30), graph.core.start()).await??;
        for result in &graph.rows {
            assert_eq!(rows(result)?, [json!({"id":1,"value":"initial"})]);
        }
        for owner in &graph.owners {
            assert!(owner.snapshot().checkpoints.is_empty());
        }
        Ok(())
    }
    .await;
    finish(graph, &mut database, proof).await
}

async fn snapshot_cleanup(dynamic: bool) -> Result<()> {
    let mut database = Database::new().await?;
    database.initialize().await?;
    database
        .sql("INSERT INTO items VALUES(2,'second'),(3,'third'),(4,'fourth')")
        .await?;
    let plugins = libraries(dynamic)?;
    let mut settings = database.source_settings();
    settings["replay"] = Value::Null;
    settings["start"] = json!({"mode":"end"});
    settings["io_timeout_ms"] = json!(500);
    let factory = plugins[1]
        .bootstrap_factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == drasi_computation_bootstrap_mysql::MYSQL_STANDALONE_BOOTSTRAP
        })
        .context("standalone factory")?;
    let provider = factory.create_provider(
        id("snapshot-owner"),
        json!({"source":"source","stream":"changes","settings":settings}),
        Some(scope()),
        None,
    )?;
    let mut paused = false;
    let proof = async {
        provider.prepare().await?;
        let mut snapshot = provider.snapshot().await?;
        let first = tokio::time::timeout(Duration::from_secs(5), snapshot.changes.next())
            .await?
            .context("first real snapshot row")??;
        anyhow::ensure!(!first.changes().is_empty(), "snapshot emitted no row");
        let pause = tokio::process::Command::new("podman")
            .args(["pause", &database.container])
            .output()
            .await?;
        anyhow::ensure!(
            pause.status.success(),
            "failed to pause owned snapshot fixture"
        );
        paused = true;
        drop(snapshot);
        anyhow::ensure!(
            tokio::time::timeout(Duration::from_millis(100), provider.stop())
                .await
                .is_err(),
            "paused database cleanup was reported complete"
        );
        anyhow::ensure!(
            provider.snapshot().await.is_err(),
            "pending cleanup permitted a replacement snapshot"
        );
        Ok(())
    }
    .await;
    let resumed = if paused {
        let result = tokio::process::Command::new("podman")
            .args(["unpause", &database.container])
            .output()
            .await?;
        anyhow::ensure!(
            result.status.success(),
            "failed to unpause owned snapshot fixture"
        );
        Ok(())
    } else {
        Ok(())
    };
    let stopped = tokio::time::timeout(Duration::from_secs(10), provider.stop())
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result);
    combine(
        "interrupted standalone MySQL snapshot",
        [proof, resumed, stopped],
    )?;
    let proof = async {
        provider.prepare().await?;
        let mut snapshot = provider.snapshot().await?;
        let mut count = 0;
        while let Some(row) =
            tokio::time::timeout(Duration::from_secs(5), snapshot.changes.next()).await?
        {
            row?;
            count += 1;
        }
        anyhow::ensure!(count == 4, "snapshot restart omitted rows");
        anyhow::ensure!(
            provider.complete_snapshot().await?.is_empty(),
            "standalone snapshot invented a watermark"
        );
        anyhow::ensure!(
            provider.completion_state()?.is_none(),
            "standalone snapshot invented durable handover state"
        );
        Ok(())
    }
    .await;
    let stopped = provider.stop().await;
    let sessions = database.expect_count(
        "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='root' AND ID<>CONNECTION_ID()","0",
    ).await;
    let removed = database.cleanup();
    combine(
        "standalone MySQL snapshot restart and cleanup",
        [proof, stopped, sessions, removed],
    )
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn configured_start_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        for gtid in [false, true] {
            configured_start(dynamic, gtid).await?;
        }
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn configured_start_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        for gtid in [false, true] {
            configured_start(dynamic, gtid).await?;
        }
    }
    Ok(())
}
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn standalone_manual_keys_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        manual_keys(dynamic, false).await?;
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn standalone_manual_keys_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        manual_keys(dynamic, false).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn coordinated_manual_keys_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        manual_keys(dynamic, true).await?;
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn coordinated_manual_keys_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        manual_keys(dynamic, true).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn database_selection_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        database_wide(dynamic).await?;
        readonly_view(dynamic).await?;
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn database_selection_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        database_wide(dynamic).await?;
        readonly_view(dynamic).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn standalone_cleanup_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        snapshot_cleanup(dynamic).await?;
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn standalone_cleanup_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        snapshot_cleanup(dynamic).await?;
    }
    Ok(())
}
