// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::{Context, Result};
use drasi_core::computation::ComputationIndexProvider;
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::{
    computation::v1::*,
    context::workers::{join_owned_worker, spawn_owned_worker, WorkerCompletion},
};
use drasi_source_postgres::{
    config::{PostgresSourceConfig, SslMode},
    native::{
        PostgresTransactionConfig, PostgresTransactionSource, PostgresTransactionSourceFactory,
    },
};
use std::{
    collections::BTreeMap,
    num::{NonZeroU64, NonZeroUsize},
    path::Path,
    sync::Arc,
    time::Duration,
};
use testcontainers::{
    core::{wait::LogWaitStrategy, ContainerPort, WaitFor},
    runners::AsyncRunner,
    ContainerAsync, GenericImage, ImageExt,
};
use tokio::sync::RwLock;
use tokio_postgres::{Client, NoTls};

#[path = "native_transactions/snapshot.rs"]
mod snapshot;
#[path = "native_transactions/tls.rs"]
mod tls;

struct Database {
    container: ContainerAsync<GenericImage>,
    client: Option<Client>,
    driver: RwLock<Option<tokio::task::JoinHandle<std::result::Result<(), tokio_postgres::Error>>>>,
    config: PostgresTransactionConfig,
}

impl Database {
    async fn new() -> Result<Self> {
        let container = GenericImage::new("postgres", "16-alpine")
            .with_exposed_port(ContainerPort::Tcp(5432))
            .with_wait_for(WaitFor::log(
                LogWaitStrategy::stdout_or_stderr("database system is ready to accept connections")
                    .with_times(2),
            ))
            .with_env_var("POSTGRES_PASSWORD", "postgres")
            .with_cmd(["-c", "wal_level=logical", "-c", "wal_sender_timeout=1500"])
            .start()
            .await?;
        Self::from_container(container).await
    }

    async fn from_container(container: ContainerAsync<GenericImage>) -> Result<Self> {
        let host = container.get_host().await?.to_string();
        let port = container.get_host_port_ipv4(5432).await?;
        let mut settings = tokio_postgres::Config::new();
        settings
            .host(&host)
            .port(port)
            .user("postgres")
            .password("postgres")
            .dbname("postgres");
        let (client, connection) = settings.connect(NoTls).await?;
        let driver = RwLock::new(None);
        spawn_owned_worker(&driver, connection).await?;
        client
            .batch_execute(
                "CREATE TABLE person (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE PUBLICATION native_publication FOR TABLE person;",
            )
            .await?;
        let row = client.query_one(
            "SELECT lsn::text FROM pg_create_logical_replication_slot('native_slot', 'pgoutput')", &[],
        ).await?;
        let config = PostgresTransactionConfig {
            connection: PostgresSourceConfig {
                host,
                port,
                database: "postgres".into(),
                user: "postgres".into(),
                password: "postgres".into(),
                tables: vec!["person".into()],
                slot_name: "native_slot".into(),
                publication_name: "native_publication".into(),
                ssl_mode: SslMode::Prefer,
                table_keys: vec![],
            },
            tls_ca_pem: None,
            start_lsn: row.get(0),
            transactions: limits(16),
            max_protocol_bytes: size(1 << 20),
            io_timeout_ms: NonZeroU64::new(5000).expect("timeout"),
            feedback_interval_ms: NonZeroU64::new(100).expect("heartbeat"),
        };
        Ok(Self {
            container,
            client: Some(client),
            driver,
            config,
        })
    }
    fn client(&self) -> &Client {
        self.client.as_ref().expect("live database client")
    }
    async fn feedback(&self) -> Result<String> {
        Ok(self.client().query_one(
            "SELECT confirmed_flush_lsn::text FROM pg_replication_slots WHERE slot_name = 'native_slot'", &[],
        ).await?.get(0))
    }
    async fn wait_feedback(&self, position: u64) -> Result<()> {
        self.wait_slot_feedback("native_slot", position).await
    }
    async fn wait_slot_feedback(&self, slot: &str, position: u64) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let value: String = self.client().query_one(
                    "SELECT confirmed_flush_lsn::text FROM pg_replication_slots WHERE slot_name = $1", &[&slot],
                ).await?.get(0);
                if lsn(&value) == position {
                    return Result::<()>::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        Ok(())
    }
    async fn shutdown(mut self) -> Result<()> {
        self.client.take();
        match join_owned_worker(self.driver.get_mut(), Duration::from_secs(5)).await? {
            WorkerCompletion::Completed(result) => result?,
            other => anyhow::bail!("database driver was not joined: {other:?}"),
        }
        self.container.stop().await?;
        Ok(())
    }
}

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("positive limit")
}
fn limits(count: usize) -> SourceTransactionLimits {
    SourceTransactionLimits {
        max_changes: size(count),
        max_bytes: size(1 << 20),
        max_duration_ms: NonZeroU64::new(5000).expect("deadline"),
    }
}
fn lsn(value: &str) -> u64 {
    let (high, low) = value.split_once('/').expect("LSN");
    (u64::from_str_radix(high, 16).expect("high LSN") << 32)
        | u64::from_str_radix(low, 16).expect("low LSN")
}
fn progress() -> Arc<QuerySourceProgress> {
    Arc::new(
        QuerySourceProgress::new(
            "native-postgres",
            ComponentId::try_new("query").expect("query"),
        )
        .expect("progress"),
    )
}
async fn query(
    path: &Path,
    progress: Arc<QuerySourceProgress>,
) -> Result<ContinuousQueryTransformer> {
    query_with_recovery(path, progress, QueryRecoveryPolicy::Strict).await
}

async fn query_with_recovery(
    path: &Path,
    progress: Arc<QuerySourceProgress>,
    recovery: QueryRecoveryPolicy,
) -> Result<ContinuousQueryTransformer> {
    let provider: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
        ),
    ));
    ContinuousQueryTransformer::new_configured(
        ContinuousQueryDefinition {
            graph_id: "native-postgres".into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:person) RETURN n.id AS id, n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("results")?,
            outbox_capacity: size(16),
        },
        provider,
        QueryOptions {
            recovery,
            ..Default::default()
        },
        QueryExecutionSettings {
            source_transactions: Some(limits(16)),
            ..Default::default()
        },
        None,
    )
    .await?
    .with_source_progress(progress)
}
fn source(
    config: PostgresTransactionConfig,
    progress: Arc<QuerySourceProgress>,
) -> Result<PostgresTransactionSource> {
    PostgresTransactionSource::new(
        ComponentId::try_new("source")?,
        StreamId::try_new("changes")?,
        config,
        progress,
    )
}
async fn next(source: &mut PostgresTransactionSource) -> Result<OutputEnvelope> {
    tokio::time::timeout(Duration::from_secs(8), source.next())
        .await??
        .context("source exhausted")
}
async fn apply(
    query: &mut ContinuousQueryTransformer,
    output: OutputEnvelope,
) -> Result<Vec<OutputEnvelope>> {
    let results = query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: output.envelope,
        })
        .await?;
    query.delivery_completed(&results).await?;
    Ok(results)
}
fn rows(query: &ContinuousQueryTransformer) -> Vec<serde_json::Value> {
    let mut rows = query
        .results()
        .snapshot()
        .expect("result snapshot")
        .rows
        .values()
        .map(|record| {
            let row = QueryChangeCodec::decode_row(record).expect("result row");
            QueryChangeCodec::row_values_to_json(&row.values)
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|row| row.to_string());
    rows
}
fn committed(progress: &QuerySourceProgress) -> u64 {
    progress.snapshot().checkpoints[&SourceProgressKey::Source("source".into())].sequence
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_whole_commits_hide_intermediate_rows_and_handle_key_changes() -> Result<()> {
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(path.path(), progress.clone()).await?;
    query.start().await?;
    let mut source = source(database.config.clone(), progress.clone())?;
    source.start().await?;
    database
        .client()
        .batch_execute(
            "BEGIN; INSERT INTO person VALUES (1, 'first');
         UPDATE person SET name = 'middle' WHERE id = 1;
         UPDATE person SET name = 'final' WHERE id = 1;
         INSERT INTO person VALUES (2, 'temporary'); DELETE FROM person WHERE id = 2; COMMIT;",
        )
        .await?;
    let group = next(&mut source).await?;
    assert_eq!(
        SourceTransactionCodec::decode(&group.envelope, limits(16))?
            .into_changes()
            .len(),
        5
    );
    assert_eq!(database.feedback().await?, database.config.start_lsn);
    assert!(rows(&query).is_empty());
    let output = apply(&mut query, group).await?;
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].envelope.changes().operations().len(), 1);
    assert_eq!(
        rows(&query),
        vec![serde_json::json!({"id":1, "name":"final"})]
    );
    database.wait_feedback(committed(&progress)).await?;
    database.client().batch_execute(
        "BEGIN; UPDATE person SET id = 3 WHERE id = 1; UPDATE person SET name = 'renamed' WHERE id = 3; COMMIT;"
    ).await?;
    let group = next(&mut source).await?;
    assert_eq!(
        SourceTransactionCodec::decode(&group.envelope, limits(16))?
            .into_changes()
            .len(),
        3
    );
    apply(&mut query, group).await?;
    assert_eq!(
        rows(&query),
        vec![serde_json::json!({"id":3, "name":"renamed"})]
    );
    database
        .client()
        .batch_execute("DELETE FROM person WHERE id = 3")
        .await?;
    let group = next(&mut source).await?;
    apply(&mut query, group).await?;
    assert!(rows(&query).is_empty());
    database.wait_feedback(committed(&progress)).await?;
    source.stop().await?;
    query.stop().await?;
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_rejected_group_is_not_acknowledged_and_higher_limit_replays_it_whole(
) -> Result<()> {
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    {
        let progress = progress();
        let mut query = query(path.path(), progress.clone()).await?;
        query.start().await?;
        let mut config = database.config.clone();
        config.transactions.max_changes = size(1);
        let mut source = source(config, progress.clone())?;
        source.start().await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES (1, 'initial')")
            .await?;
        let group = next(&mut source).await?;
        apply(&mut query, group).await?;
        let before = committed(&progress);
        database.wait_feedback(before).await?;
        database
            .client()
            .batch_execute(
                "BEGIN; UPDATE person SET name = 'final' WHERE id = 1;
             INSERT INTO person VALUES (2, 'second'); COMMIT;",
            )
            .await?;
        let error = next(&mut source).await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<SourceTransactionError>(),
            Some(SourceTransactionError::Limit { .. })
        ));
        assert_eq!(committed(&progress), before);
        assert_eq!(lsn(&database.feedback().await?), before);
        assert_eq!(
            rows(&query),
            vec![serde_json::json!({"id":1, "name":"initial"})]
        );
        source.stop().await?;
        query.stop().await?;
    }
    {
        let progress = progress();
        let mut query = query(path.path(), progress.clone()).await?;
        query.start().await?;
        let mut source = source(database.config.clone(), progress.clone())?;
        source.start().await?;
        let group = next(&mut source).await?;
        assert_eq!(
            SourceTransactionCodec::decode(&group.envelope, limits(16))?
                .into_changes()
                .len(),
            2
        );
        let duplicate = group.clone();
        apply(&mut query, group).await?;
        assert!(apply(&mut query, duplicate).await?.is_empty());
        assert_eq!(
            rows(&query),
            vec![
                serde_json::json!({"id":1, "name":"final"}),
                serde_json::json!({"id":2, "name":"second"}),
            ]
        );
        database.wait_feedback(committed(&progress)).await?;
        source.stop().await?;
        query.stop().await?;
    }
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_heartbeats_survive_backpressure_and_same_source_restart_replays_uncommitted_work(
) -> Result<()> {
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(path.path(), progress.clone()).await?;
    query.start().await?;
    let mut source = source(database.config.clone(), progress.clone())?;
    source.start().await?;
    for id in 1..=4 {
        database
            .client()
            .execute("INSERT INTO person VALUES ($1, 'row')", &[&id])
            .await?;
    }
    // Longer than the server's 1500 ms sender timeout, with the bounded output full.
    tokio::time::sleep(Duration::from_millis(2400)).await;
    let active: bool = database
        .client()
        .query_one(
            "SELECT active FROM pg_replication_slots WHERE slot_name = 'native_slot'",
            &[],
        )
        .await?
        .get(0);
    assert!(active, "feedback stopped under output backpressure");
    assert_eq!(database.feedback().await?, database.config.start_lsn);
    let first = next(&mut source).await?;
    apply(&mut query, first).await?;
    database.wait_feedback(committed(&progress)).await?;
    for expected in 2..=4 {
        let original = next(&mut source).await?;
        let original_sequence = original.envelope.system().sequence();
        let original_position = original.envelope.system().source_position().cloned();
        {
            let stopping = source.stop();
            tokio::pin!(stopping);
            assert!(
                futures::poll!(&mut stopping).is_pending(),
                "real worker join must remain owned"
            );
        }
        assert!(source
            .start()
            .await
            .unwrap_err()
            .to_string()
            .contains("cleanup"));
        source.stop().await?;
        source.start().await?;
        let replay = next(&mut source).await?;
        assert!(replay.envelope.system().sequence() > original_sequence);
        assert_eq!(
            replay.envelope.system().source_position().cloned(),
            original_position
        );
        apply(&mut query, replay).await?;
        assert_eq!(rows(&query).len(), expected);
        database.wait_feedback(committed(&progress)).await?;
    }
    source.stop().await?;
    query.stop().await?;
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_tls_requirement_and_unavailable_cursor_fail_before_publication() -> Result<()> {
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(path.path(), progress.clone()).await?;
    query.start().await?;
    let mut config = database.config.clone();
    config.connection.ssl_mode = SslMode::Require;
    let mut source_tls = source(config, progress.clone())?;
    let error = source_tls.start().await.unwrap_err();
    assert!(error.to_string().contains("refused required TLS"));
    assert!(source_tls
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("cleanup"));
    source_tls.stop().await?;
    let mut config = database.config.clone();
    config.start_lsn = "0/1".into();
    let mut source_gap = source(config, progress)?;
    assert!(source_gap
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("no longer available"));
    source_gap.stop().await?;
    database
        .client()
        .batch_execute("ALTER SYSTEM SET max_slot_wal_keep_size = '64MB';")
        .await?;
    database
        .client()
        .query_one("SELECT pg_reload_conf()", &[])
        .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let setting: String = database
                .client()
                .query_one("SHOW max_slot_wal_keep_size", &[])
                .await?
                .get(0);
            if setting == "64MB" {
                return Result::<()>::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let mut unsafe_retention = source(
        database.config.clone(),
        source_gap.recovery_progress().unwrap(),
    )?;
    assert!(unsafe_retention
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("max_slot_wal_keep_size"));
    unsafe_retention.stop().await?;
    query.stop().await?;
    database.shutdown().await
}

fn endpoint(id: &str, port: &str) -> Endpoint {
    Endpoint::new(
        ComponentId::try_new(id).expect("component"),
        PortId::try_new(port).expect("port"),
    )
}

struct Settings(serde_json::Value);

#[async_trait::async_trait]
impl ConfigurationResolver for Settings {
    fn validate_reference(&self, key: &str) -> Result<()> {
        anyhow::ensure!(key == "postgres", "unknown settings reference");
        Ok(())
    }
    async fn resolve(&self, key: &str) -> Result<serde_json::Value> {
        self.validate_reference(key)?;
        Ok(self.0.clone())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_direct_and_factory_sources_deliver_whole_groups_through_actual_graph_pipes(
) -> Result<()> {
    for (factory_created, coordinated_snapshot) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let database = Database::new().await?;
        let path = tempfile::tempdir()?;
        let progress = progress();
        let (source, bootstrap) = if coordinated_snapshot {
            let (source, bootstrap) = PostgresTransactionSource::coordinated(
                ComponentId::try_new("source")?,
                StreamId::try_new("changes")?,
                database.config.clone(),
                progress.clone(),
            )?;
            (source, Some(bootstrap))
        } else {
            (source(database.config.clone(), progress.clone())?, None)
        };
        let mut query = query(path.path(), progress.clone()).await?;
        if let Some(bootstrap) = &bootstrap {
            query = query.with_bootstrap(bootstrap.clone());
        }
        let (sink, mut received) = drasi_reaction_application::NativeApplicationReaction::channel(
            "sink",
            QueryChangeCodec::schema().descriptor().clone(),
            size(1),
        )?;
        let graph = ComputationGraph::builder("native-postgres")
            .query(Box::new(query))
            .sink(Box::new(sink));
        let graph = if factory_created {
            let factory = Arc::new(PostgresTransactionSourceFactory::default());
            let resource = ResourceId::try_new("progress")?;
            let settings = ResourceId::try_new("settings")?;
            let mut specification = ComponentSpecification {
                descriptor: PostgresTransactionSource::describe(ComponentId::try_new("source")?)?,
                role: ComponentRole::Source,
                completion: None,
                implementation: factory.descriptor().implementation.clone(),
                configuration_version: 1,
                configuration: BTreeMap::from([
                    (
                        Arc::from("stream"),
                        ConfigurationValue::Literal("changes".into()),
                    ),
                    (
                        Arc::from("settings"),
                        ConfigurationValue::Reference {
                            resource: settings.clone(),
                            key: Arc::from("postgres"),
                            secret: true,
                        },
                    ),
                ]),
                dependencies: BTreeMap::from([(
                    Arc::from("source_progress"),
                    vec![resource.clone()],
                )]),
            };
            let mut invalid = specification.clone();
            invalid.descriptor = ComponentDescriptor::try_new(
                ComponentId::try_new("source")?,
                vec![PortDescriptor::new(
                    PortId::try_new("out")?,
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?;
            assert!(factory.validate(&invalid).is_err());
            let graph = if let Some(bootstrap) = bootstrap {
                let resource = ResourceId::try_new("snapshot")?;
                specification.configuration.insert(
                    Arc::from("coordinated_snapshot"),
                    ConfigurationValue::Literal(true.into()),
                );
                specification
                    .dependencies
                    .insert(Arc::from("snapshot"), vec![resource.clone()]);
                graph
                    .declare_resource(ResourceSpecification {
                        id: resource.clone(),
                        role: ResourceRole::Bootstrap,
                        ownership: ResourceOwnership::Borrowed,
                        binding: Arc::from("snapshot"),
                    })?
                    .provide_resource(
                        resource,
                        ResourceHandle::new(ResourceRole::Bootstrap, bootstrap),
                    )?
            } else {
                graph
            };
            graph
                .declare_resource(ResourceSpecification {
                    id: settings.clone(),
                    role: ResourceRole::SecretStore,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from("settings"),
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
                .declare_resource(ResourceSpecification {
                    id: resource.clone(),
                    role: ResourceRole::Checkpoint,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from("progress"),
                })?
                .provide_resource(
                    resource,
                    ResourceHandle::new(
                        ResourceRole::Checkpoint,
                        Arc::new(QuerySourceProgressResource(progress.clone())),
                    ),
                )?
                .component(specification, factory)
        } else {
            graph.source(Box::new(source))
        };
        let mut graph = graph
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
        let run = graph.run()?;
        let control = run.control();
        let (run_result, result) = tokio::time::timeout(Duration::from_secs(20), async {
            tokio::join!(run, async {
                let exercise = async {
                    let report = control.start_components(GraphRevision(1), GraphSelection::All).await?;
                    anyhow::ensure!(report.summary == OperationSummary::Completed, "graph startup: {report:?}");
                    database.client().batch_execute(
                        "BEGIN; INSERT INTO person VALUES (1, 'first'); UPDATE person SET name = 'last'; COMMIT;"
                    ).await?;
                    let output = received.recv().await.context("application sink closed")?;
                    let [ChangeOperation::Added { after, .. }] = output.envelope.changes().operations() else {
                        anyhow::bail!("expected only one final row");
                    };
                    let row = QueryChangeCodec::decode_row(after)?;
                    assert_eq!(QueryChangeCodec::row_values_to_json(&row.values), serde_json::json!({"id":1,"name":"last"}));
                    let slot = if coordinated_snapshot { snapshot::managed_slot(&database).await? } else { "native_slot".into() };
                    database.wait_slot_feedback(&slot, committed(&progress)).await?;
                    let report = control.stop_components(GraphRevision(1), GraphSelection::All).await?;
                    anyhow::ensure!(report.summary == OperationSummary::Completed, "graph stop: {report:?}");
                    Result::<()>::Ok(())
                }.await;
                control.cancel();
                exercise
            })
        }).await?;
        result?;
        assert!(matches!(run_result, Err(GraphError::Cancelled)));
        graph.shutdown().await?;
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_process_exit_helper() -> Result<()> {
    let Ok(mode) = std::env::var("DRASI_PG_TRANSACTION_CRASH") else {
        return Ok(());
    };
    anyhow::ensure!(mode == "before" || mode == "after", "invalid crash point");
    let config: PostgresTransactionConfig =
        serde_json::from_str(&std::env::var("DRASI_PG_TRANSACTION_CONFIG")?)?;
    let path = std::env::var("DRASI_PG_TRANSACTION_PATH")?;
    let progress = progress();
    let mut query = query(Path::new(&path), progress.clone()).await?;
    query.start().await?;
    let mut source = source(config, progress)?;
    source.start().await?;
    let group = next(&mut source).await?;
    if mode == "after" {
        apply(&mut query, group).await?;
    }
    // Exit synchronously on current_thread: no post-commit feedback task can run.
    std::process::exit(82)
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_abrupt_process_exit_before_and_after_query_commit_recovers_without_partial_rows(
) -> Result<()> {
    for point in ["before", "after"] {
        let database = Database::new().await?;
        let path = tempfile::tempdir()?;
        database.client().batch_execute(
            "BEGIN; INSERT INTO person VALUES (1, 'first'); UPDATE person SET name = 'last'; COMMIT;"
        ).await?;
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::process::Command::new(std::env::current_exe()?)
                .args(["--exact", "postgres_process_exit_helper", "--nocapture"])
                .env("DRASI_PG_TRANSACTION_CRASH", point)
                .env(
                    "DRASI_PG_TRANSACTION_CONFIG",
                    serde_json::to_string(&database.config)?,
                )
                .env("DRASI_PG_TRANSACTION_PATH", path.path())
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        anyhow::ensure!(
            output.status.code() == Some(82),
            "crash helper failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let active: bool = database
                    .client()
                    .query_one(
                        "SELECT active FROM pg_replication_slots WHERE slot_name = 'native_slot'",
                        &[],
                    )
                    .await?
                    .get(0);
                if !active {
                    return Result::<()>::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert_eq!(database.feedback().await?, database.config.start_lsn);
        let progress = progress();
        let mut query = query(path.path(), progress.clone()).await?;
        query.start().await?;
        assert_eq!(rows(&query).len(), usize::from(point == "after"));
        let mut source = source(database.config.clone(), progress.clone())?;
        source.start().await?;
        if point == "before" {
            let replay = next(&mut source).await?;
            apply(&mut query, replay).await?;
        }
        assert_eq!(
            rows(&query),
            vec![serde_json::json!({"id":1,"name":"last"})]
        );
        database
            .client()
            .batch_execute("INSERT INTO person VALUES (2, 'next')")
            .await?;
        let next_group = next(&mut source).await?;
        let changes =
            SourceTransactionCodec::decode(&next_group.envelope, limits(16))?.into_changes();
        assert_eq!(
            changes.len(),
            1,
            "already committed group was not suppressed on restart"
        );
        apply(&mut query, next_group).await?;
        assert_eq!(rows(&query).len(), 2);
        database.wait_feedback(committed(&progress)).await?;
        source.stop().await?;
        query.stop().await?;
        database.shutdown().await?;
    }
    Ok(())
}
