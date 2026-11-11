// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_source_sqlite::{SqliteConfig, SqliteOutput, SqliteReplayConfig};
use drasi_host_sdk::computation::{
    NativeBootstrapProxy, NativeSourceBootstrapProgress, NativeSourceBootstrapResource,
};
use drasi_lib::DrasiLib;
use std::{
    net::TcpListener,
    num::{NonZeroU64, NonZeroUsize},
};

#[path = "durable_chain.rs"]
mod chain;
#[path = "durable_bootstrap_crash.rs"]
mod crashes;
#[path = "synthetic_joins.rs"]
mod joins;
#[path = "query_subscriptions.rs"]
mod subscriptions;

struct Graph {
    core: DrasiLib,
    query: ComponentHandle,
    source: ComponentHandle,
    sink: ComponentHandle,
    outlet: Option<ComponentHandle>,
    catalog: Option<QueryResultsCatalog>,
    progress: Arc<QuerySourceProgress>,
    bootstrap_state: Arc<dyn BootstrapState>,
    provider: Arc<NativeBootstrapProxy>,
    _coordinator: Arc<NativeSourceBootstrapResource>,
    received: Arc<Mutex<Vec<ChangeEnvelope>>>,
    changed: Arc<Notify>,
    url: String,
}

fn limits() -> SourceTransactionLimits {
    SourceTransactionLimits {
        max_changes: NonZeroUsize::new(64).expect("positive change bound"),
        max_bytes: NonZeroUsize::new(1 << 20).expect("positive byte bound"),
        max_duration_ms: NonZeroU64::new(5000).expect("positive deadline"),
    }
}

impl Graph {
    async fn new(database: &Path, indexes: &Path, generation: u64) -> Result<Self> {
        Self::with_gate(database, indexes, generation, None).await
    }

    async fn with_gate(
        database: &Path,
        indexes: &Path,
        generation: u64,
        gate: Option<Arc<super::lifecycle::Gate>>,
    ) -> Result<Self> {
        let configuration = json!({
            "query": TEXT, "bootstrap": true, "source_transactions": limits(),
            "recovery": if gate.is_some() { "auto_reset" } else { "strict" }
        });
        Self::with_configuration(
            database,
            indexes,
            generation,
            gate,
            configuration,
            vec!["items".into()],
        )
        .await
    }

    async fn with_configuration(
        database: &Path,
        indexes: &Path,
        generation: u64,
        gate: Option<Arc<super::lifecycle::Gate>>,
        configuration: serde_json::Value,
        tables: Vec<String>,
    ) -> Result<Self> {
        Self::with_delivery(
            database,
            indexes,
            generation,
            gate,
            configuration,
            tables,
            None,
        )
        .await
    }

    async fn with_delivery(
        database: &Path,
        indexes: &Path,
        generation: u64,
        gate: Option<Arc<super::lifecycle::Gate>>,
        configuration: serde_json::Value,
        tables: Vec<String>,
        delivery: Option<(drasi_lib::DispatchMode, usize)>,
    ) -> Result<Self> {
        let core = DrasiLib::builder().with_id(GRAPH).build().await?;
        core.start().await?;
        let scope = Scope {
            instance_id: GRAPH.into(),
            graph_id: core.output_catalogue()?.graph_id().to_string(),
            generation,
        };
        let mut query = factory()?.create_component_with_processing(
            ComponentId::try_new(COMPONENT)?,
            configuration,
            scope.clone(),
            provider(indexes)?,
        )?;
        let catalog = match delivery {
            Some((mode, capacity)) => {
                let catalog = QueryResultsCatalog::new(scope.graph_id.as_str())?;
                query.bind_query_catalog(catalog.clone(), mode, capacity)?;
                Some(catalog)
            }
            None => None,
        };
        let progress = query
            .processing_progress_owner()
            .context("missing local processing owner")?;
        let bootstrap_state = query.bootstrap_state().context("missing bootstrap state")?;
        let source_library = library().with_file_name(format!(
            "{}drasi_computation_source_sqlite{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        ));
        let source_plugin = computation::load(source_library)?;
        let source_factory = source_plugin
            .factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == "drasi.sqlite/replay-transactions"
            })
            .context("missing replaying SQLite source")?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let settings = SqliteConfig {
            path: Some(database.to_path_buf()),
            tables,
            table_keys: Vec::new(),
            output: SqliteOutput::Transactions,
            replay: Some(SqliteReplayConfig {
                max_transactions: NonZeroUsize::new(16).expect("positive retention bound"),
                max_bytes: NonZeroUsize::new(8 << 20).expect("positive byte bound"),
            }),
            transactions: limits(),
            max_sql_bytes: NonZeroUsize::new(65536).expect("positive SQL bound"),
            command_capacity: NonZeroUsize::new(8).expect("positive command capacity"),
            output_capacity: NonZeroUsize::new(1).expect("positive output capacity"),
            shutdown_timeout_ms: NonZeroU64::new(5000).expect("positive cleanup deadline"),
        };
        let configuration = json!({
            "stream": "sqlite/out", "settings": settings, "coordinated_snapshot": true,
            "rest_api": {"host":"127.0.0.1", "port":port}
        });
        let coordinator = source_factory.create_source_bootstrap(
            ComponentId::try_new("sqlite")?,
            configuration.clone(),
            scope.clone(),
            NativeSourceBootstrapProgress::Single(progress.clone()),
        )?;
        let source = source_factory.create_component_with_source_bootstrap(
            ComponentId::try_new("sqlite")?,
            configuration,
            scope.clone(),
            coordinator.clone(),
            NativeSourceBootstrapProgress::Single(progress.clone()),
        )?;
        let bootstrap_plugin = computation::load(sqlite::bootstrap_library())?;
        let bootstrap_factory = bootstrap_plugin
            .bootstrap_factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == "drasi.sqlite/coordinated-bootstrap"
            })
            .context("missing coordinated SQLite bootstrap")?;
        let provider = bootstrap_factory.create_paired_query_provider(
            ComponentId::try_new(COMPONENT)?,
            json!({"source":"sqlite", "stream":"sqlite/out"}),
            scope.clone(),
            progress.clone(),
            coordinator.clone(),
        )?;
        let bound: Arc<dyn ComputationBootstrapProvider> = match gate {
            Some(gate) => Arc::new(super::lifecycle::PausedProvider {
                inner: provider.clone(),
                gate,
            }),
            None => provider.clone(),
        };
        query.bind_bootstrap(Arc::new(NativeBootstrapResource(bound)))?;
        query.prepare_processing().await?;
        let query = core
            .add_computation_component(
                ComponentAddition::new(ConstructedComponent::transformer(Box::new(query)))
                    .auto_start(false)
                    .bind_stream(
                        PortId::try_new("out")?,
                        StreamId::try_new(format!("{COMPONENT}/out"))?,
                    ),
            )
            .await?;
        let source = core
            .add_computation_component(
                ComponentAddition::new(ConstructedComponent::source(Box::new(source)))
                    .auto_start(false)
                    .bind_stream(PortId::try_new("out")?, StreamId::try_new("sqlite/out")?),
            )
            .await?;
        let received = Arc::new(Mutex::new(Vec::new()));
        let changed = Arc::new(Notify::new());
        let sink = core
            .add_computation_component(
                ComponentAddition::new(ConstructedComponent::sink(Box::new(Sink {
                    descriptor: ComponentDescriptor::try_new(
                        ComponentId::try_new("results")?,
                        vec![port_descriptor("in", PortDirection::Input)?],
                    )?,
                    received: received.clone(),
                    notification: changed.clone(),
                })))
                .auto_start(false),
            )
            .await?;
        let mut edges = vec![
            EdgeDefinition::new(
                Endpoint::new(source.id().clone(), PortId::try_new("out")?),
                Endpoint::new(query.id().clone(), PortId::try_new("in")?),
            ),
            EdgeDefinition::new(
                Endpoint::new(query.id().clone(), PortId::try_new("out")?),
                Endpoint::new(sink.id().clone(), PortId::try_new("in")?),
            ),
        ];
        let outlet = if let Some(catalog) = &catalog {
            let factory = Arc::new(QueryResultsOutletFactory::default());
            let resource = ResourceId::try_new("native-query-results")?;
            let mut specification = factory.specification_from_config(
                ComponentId::try_new("query-result-outlet")?,
                json!({}),
            )?;
            specification
                .dependencies
                .insert("catalog".into(), vec![resource.clone()]);
            let outlet = core
                .add_computation_component(
                    ComponentAddition::from_specification(specification, factory)
                        .auto_start(false)
                        .with_resource(
                            ResourceSpecification {
                                id: resource,
                                role: ResourceRole::QueryCatalog,
                                ownership: ResourceOwnership::Graph,
                                binding: "native-query-results".into(),
                            },
                            ResourceHandle::new(
                                ResourceRole::QueryCatalog,
                                Arc::new(catalog.clone()),
                            ),
                        ),
                )
                .await?;
            edges.push(EdgeDefinition::new(
                Endpoint::new(query.id().clone(), PortId::try_new("out")?),
                Endpoint::new(outlet.id().clone(), PortId::try_new("in")?),
            ));
            Some(outlet)
        } else {
            None
        };
        crate::bootstrap::connect(&core, edges).await?;
        Ok(Self {
            core,
            query,
            source,
            sink,
            outlet,
            catalog,
            progress,
            bootstrap_state,
            provider,
            _coordinator: coordinator,
            received,
            changed,
            url: format!("http://127.0.0.1:{port}/api/batch"),
        })
    }

    async fn start(&self) -> Result<()> {
        self.sink.start().await?;
        if let Some(outlet) = &self.outlet {
            outlet.start().await?;
        }
        self.query.start().await?;
        self.provider.freeze_for_retirement()?.resume();
        self.source.start().await?;
        Ok(())
    }

    async fn snapshot(&self) -> Result<OutputSnapshot> {
        Ok(self
            .core
            .output_catalogue()?
            .resolve(&OutputEndpoint::new(
                self.query.id().clone(),
                PortId::try_new("out")?,
            ))?
            .snapshot()
            .await?)
    }

    async fn output(&self, index: usize) -> Result<ChangeEnvelope> {
        loop {
            let changed = self.changed.notified();
            if let Some(output) = self
                .received
                .lock()
                .map_err(|_| anyhow::anyhow!("result capture poisoned"))?
                .get(index)
                .cloned()
            {
                return Ok(output);
            }
            changed.await;
        }
    }

    async fn apply(&self, operations: serde_json::Value) -> Result<()> {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()?
            .post(&self.url)
            .json(&json!({"operations": operations}))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

async fn interrupted_coordinated_bootstrap() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    let indexes = directory.path().join("indexes");
    sqlite::create_database(&database)?;
    let gate = super::lifecycle::Gate::new(false);
    let graph = Graph::with_gate(&database, &indexes, 1, Some(gate.clone())).await?;
    let exercise = async {
        graph.sink.start().await?;
        let intent = {
            let start = graph.query.start();
            tokio::pin!(start);
            tokio::select! {
                result = &mut start => anyhow::bail!("initialization did not pause after its first row: {result:?}"),
                _ = gate.reached.notified() => {}
            }
            assert!(graph.snapshot().await.is_err());
            assert!(!graph.progress.snapshot().bootstrap_complete);
            let state = graph.bootstrap_state.read().await?.context("provider intent is missing")?;
            let intent: serde_json::Value = serde_json::from_slice(&state)?;
            assert_eq!(intent["completed"], false);
            intent
        };
        graph.query.stop().await?;
        assert!(graph.bootstrap_state.read().await.is_err());
        graph.provider.freeze_for_retirement()?.resume();
        gate.release();
        graph.query.start().await?;
        assert_eq!(total(&graph.snapshot().await?)?, 30.0);
        assert_eq!(graph.progress.snapshot().reset_generation, 1);
        let completed: serde_json::Value = serde_json::from_slice(
            &graph.bootstrap_state.read().await?.context("provider completion is missing")?)?;
        assert_eq!(completed["completed"], true);
        assert_eq!(completed["epoch"], intent["epoch"], "reset lost the unfinished provider's actual ownership");
        graph.source.start().await?;
        graph.apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":14}}])).await?;
        let output = graph.output(0).await?;
        aggregate(&[OutputEnvelope { port: PortId::try_new("out")?, envelope: output }],
            Some(30.0), 34.0, 1)?;
        Ok::<_, anyhow::Error>(())
    }.await;
    let stopped = graph.core.shutdown().await;
    exercise?;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_coordinated_sqlite_partial_bootstrap_preserves_intent_current_thread() -> Result<()>
{
    tokio::time::timeout(Duration::from_secs(30), interrupted_coordinated_bootstrap()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_coordinated_sqlite_partial_bootstrap_preserves_intent_multi_thread() -> Result<()>
{
    tokio::time::timeout(Duration::from_secs(30), interrupted_coordinated_bootstrap()).await?
}

fn port_descriptor(name: &str, direction: PortDirection) -> Result<PortDescriptor> {
    Ok(PortDescriptor::new(
        PortId::try_new(name)?,
        direction,
        GraphChangeCodec::schema().descriptor().clone(),
        PipeRequirements::default(),
    ))
}

async fn snapshot_live_reopen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    let indexes = directory.path().join("indexes");
    sqlite::create_database(&database)?;
    let graph = Graph::new(&database, &indexes, 1).await?;
    let first = async {
        let report = graph
            .core
            .computation_control()?
            .recovery_report(RecoveryRequirement {
                consumer: graph.query.id().clone(),
                scope: RecoveryScope::Failure(drasi_core::interface::FailureMode::ProcessRestart),
                guarantees: [
                    RecoveryGuarantee::Replay,
                    RecoveryGuarantee::CommittedProcessing,
                ]
                .into_iter()
                .collect(),
            })
            .await?;
        anyhow::ensure!(
            report.issues.is_empty(),
            "actual native recovery path is incomplete: {report:?}"
        );
        graph.start().await?;
        let initial = graph.snapshot().await?;
        assert_eq!(total(&initial)?, 30.0);
        assert_eq!(initial.cursor().sequence(), 0);
        let progress = graph.progress.snapshot();
        assert!(
            progress.recovered
                && progress.bootstrap_complete
                && progress.ready
                && progress.persistent
        );
        let checkpoint = progress
            .checkpoints
            .get(&SourceProgressKey::Source("sqlite".into()))
            .context("missing initialization source watermark")?;
        assert_eq!(checkpoint.sequence, 0);
        assert!(checkpoint
            .source_position
            .as_ref()
            .is_some_and(|position| !position.is_empty()));
        assert!(
            progress.transport_sequences.is_empty(),
            "snapshot ordinals must not become consumed input"
        );
        graph
            .apply(json!([
                {"op":"update","table":"items","id":"1","data":{"amount":11}},
                {"op":"update","table":"items","id":"1","data":{"amount":12}},
                {"op":"insert","table":"items","data":{"id":3,"amount":7}}
            ]))
            .await?;
        let output = graph.output(0).await?;
        aggregate(
            &[OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: output,
            }],
            Some(30.0),
            39.0,
            1,
        )?;
        let progress = graph.progress.snapshot();
        assert_eq!(
            progress.checkpoints[&SourceProgressKey::Source("sqlite".into())].sequence,
            1
        );
        assert_eq!(total(&graph.snapshot().await?)?, 39.0);
        Ok::<_, anyhow::Error>(initial.cursor().producer().clone())
    }
    .await;
    let stopped = graph.core.shutdown().await;
    let producer = first?;
    stopped?;
    drop(graph);
    let sum: i64 = rusqlite::Connection::open(&database)?.query_row(
        "SELECT sum(amount) FROM items",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(sum, 39);

    let graph = Graph::new(&database, &indexes, 2).await?;
    let resumed = async {
        graph.start().await?;
        let snapshot = graph.snapshot().await?;
        assert_eq!(total(&snapshot)?, 39.0);
        assert_eq!(snapshot.cursor().sequence(), 1);
        assert_eq!(snapshot.cursor().producer(), &producer);
        assert_eq!(
            graph.progress.snapshot().checkpoints[&SourceProgressKey::Source("sqlite".into())]
                .sequence,
            1
        );
        graph
            .apply(json!([{"op":"update","table":"items","id":"2","data":{"amount":21}}]))
            .await?;
        let output = graph.output(0).await?;
        aggregate(
            &[OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: output,
            }],
            Some(39.0),
            40.0,
            2,
        )?;
        assert_eq!(total(&graph.snapshot().await?)?, 40.0);
        assert_eq!(
            graph.progress.snapshot().checkpoints[&SourceProgressKey::Source("sqlite".into())]
                .sequence,
            2
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = graph.core.shutdown().await;
    resumed?;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_coordinated_sqlite_snapshot_live_and_reopen_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), snapshot_live_reopen()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_coordinated_sqlite_snapshot_live_and_reopen_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), snapshot_live_reopen()).await?
}

async fn ordinary_read_api() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let graph = Graph::new(&database, &directory.path().join("indexes"), 1).await?;
    let exercise = async {
        graph.start().await?;
        let query = graph
            .core
            .query_manager()
            .get_query_instance(COMPONENT)
            .await?;
        assert_eq!(query.get_config().query, TEXT);
        assert!(!query.is_volatile());
        let initial = query.fetch_snapshot().await?;
        assert_eq!(initial.as_of_sequence, 0);
        assert_eq!(initial.to_vec(), vec![json!({"total":30.0})]);
        assert_eq!(
            graph.core.get_query_results(COMPONENT).await?,
            initial.to_vec()
        );
        assert_eq!(graph.core.get_query_config(COMPONENT).await?.query, TEXT);
        graph
            .apply(json!([{"op":"update","table":"items","id":"1","data":{"amount":12}}]))
            .await?;
        graph.output(0).await?;
        let snapshot = query.fetch_snapshot().await?;
        assert_eq!(snapshot.to_vec(), vec![json!({"total":32.0})]);
        assert_eq!(snapshot.as_of_sequence, 1);
        let outbox = query.fetch_outbox(0).await?;
        assert_eq!(outbox.results.len(), 1);
        assert_eq!(outbox.latest_sequence, 1);
        assert_eq!(outbox.config_hash, snapshot.config_hash);
        assert_eq!(outbox.output_generation, snapshot.output_generation);
        assert_eq!(query.output_generation().await, snapshot.output_generation);
        let head = query.fetch_outbox(u64::MAX).await?;
        assert!(head.results.is_empty());
        assert_eq!(head.latest_sequence, 1);
        graph.query.stop().await?;
        assert!(query.fetch_snapshot().await.is_err());
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = graph.core.shutdown().await;
    exercise?;
    stopped?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_query_uses_ordinary_library_read_api_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), ordinary_read_api()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_query_uses_ordinary_library_read_api_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), ordinary_read_api()).await?
}
