// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_host_sdk::computation::{
    NativeBootstrapFactory, NativeBootstrapNodeFactory, NativeSourceBootstrapProgress,
    NativeSourceBootstrapResource,
};
use futures_util::StreamExt;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

struct SnapshotGate {
    permits: Arc<tokio::sync::Semaphore>,
    requests: AtomicUsize,
}

struct GatedProvider {
    inner: Arc<dyn ComputationBootstrapProvider>,
    gate: Option<Arc<SnapshotGate>>,
}

#[async_trait::async_trait]
impl ComputationBootstrapProvider for GatedProvider {
    fn freeze_for_retirement(&self) -> Result<Box<dyn BootstrapRetirement>> {
        self.inner.freeze_for_retirement()
    }
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        self.inner.validate_query_scope(instance, graph, query)
    }
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.inner.recovery_reader()
    }
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.inner.prepare_with_state(state).await
    }
    fn has_pending_snapshot(&self) -> Result<bool> {
        self.inner.has_pending_snapshot()
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot().await
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        let mut snapshot = self.inner.snapshot_with_state(state).await?;
        if let Some(gate) = &self.gate {
            let gate = gate.clone();
            snapshot.changes = Box::pin(futures_util::stream::try_unfold(
                (snapshot.changes, gate),
                |(mut stream, gate)| async move {
                    gate.requests.fetch_add(1, Ordering::SeqCst);
                    gate.permits.clone().acquire_owned().await?.forget();
                    Ok(stream
                        .next()
                        .await
                        .transpose()?
                        .map(|input| (input, (stream, gate))))
                },
            ));
        }
        Ok(snapshot)
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

#[async_trait::async_trait]
impl ResourceCleanup for GatedProvider {
    async fn shutdown(&self) -> Result<()> {
        self.inner.stop().await
    }
}

struct Options {
    shared: bool,
    names: Vec<&'static str>,
    max_bytes: usize,
    recovery: QueryRecoveryPolicy,
    gates: Option<Vec<Arc<SnapshotGate>>>,
    supersede_stopped: bool,
}

impl Options {
    fn new(shared: bool) -> Self {
        Self {
            shared,
            names: if shared {
                vec!["first", "second"]
            } else {
                vec!["first"]
            },
            max_bytes: 1 << 20,
            recovery: QueryRecoveryPolicy::Strict,
            gates: None,
            supersede_stopped: false,
        }
    }
}

struct Graph {
    core: Arc<DrasiLib>,
    owners: Vec<Arc<QuerySourceProgress>>,
    names: Vec<&'static str>,
    superseded: Superseded,
}

type Superseded = Arc<std::sync::Mutex<Vec<Arc<dyn ComputationBootstrapProvider>>>>;

/// A graph bootstrap provider node for the paired SQLite factory, connected
/// to the source's bootstrap input. Each session resolves the coordinator the
/// source owns, exactly as `NativeBootstrapNodeFactory` does, and adds the
/// fixture's snapshot gates and superseded providers.
struct PairedProviderNode {
    node: NativeBootstrapNodeFactory,
    bootstrap: Arc<NativeBootstrapFactory>,
    gates: BTreeMap<ComponentId, Arc<SnapshotGate>>,
    supersede_stopped: bool,
    superseded: Superseded,
}

#[async_trait::async_trait]
impl ComponentFactory for PairedProviderNode {
    fn descriptor(&self) -> &FactoryDescriptor {
        self.node.descriptor()
    }

    fn describe(
        &self,
        context: &ConstructionContext,
    ) -> Result<drasi_lib::computation::v1::ComponentDescription> {
        self.node.describe(context)
    }

    fn specification_from_config(
        &self,
        id: ComponentId,
        configuration: Value,
    ) -> Result<ComponentSpecification> {
        self.node.specification_from_config(id, configuration)
    }

    fn validate(&self, specification: &ComponentSpecification) -> Result<()> {
        self.node.validate(specification)
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        self.node
            .validate(&context.specification)
            .map_err(ComponentCreationError::terminal)?;
        let configuration = serde_json::to_value(context.configuration())
            .map_err(ComponentCreationError::terminal)?;
        let scope = drasi_computation_plugin_sdk::Scope {
            instance_id: context.instance_id.to_string(),
            graph_id: context.graph_id.to_string(),
            generation: context.generation.0,
        };
        let bootstrap = self.bootstrap.clone();
        let gates = self.gates.clone();
        let supersede_stopped = self.supersede_stopped;
        let superseded = self.superseded.clone();
        let running = context.output_catalogue().clone();
        let sessions: BootstrapProviderFactory = Arc::new(move |request| {
            let origin = request.origin().clone();
            let labels = request.labels.clone();
            let source = NativeSourceBootstrapResource::registered(
                &scope.instance_id,
                &scope.graph_id,
                &request.consumer,
                Some(&running),
            )
            .context("source coordinator is not registered")?;
            let owner = source
                .owner_for(&origin)
                .context("source has no progress for the consumer")?;
            if supersede_stopped {
                let previous = bootstrap.create_paired_query_provider(
                    origin.clone(),
                    configuration.clone(),
                    scope.clone(),
                    owner.clone(),
                    source.clone(),
                    labels.clone(),
                )?;
                // A provider that never prepared stops without native work.
                futures_util::FutureExt::now_or_never(previous.stop())
                    .context("unprepared provider stop pended")??;
                superseded
                    .lock()
                    .map_err(|_| anyhow::anyhow!("superseded providers poisoned"))?
                    .push(previous);
            }
            let inner = bootstrap.create_paired_query_provider(
                origin.clone(),
                configuration.clone(),
                scope.clone(),
                owner,
                source,
                labels,
            )?;
            Ok(Arc::new(GatedProvider {
                inner,
                gate: gates.get(&origin).cloned(),
            }) as Arc<dyn ComputationBootstrapProvider>)
        });
        let description = self
            .node
            .describe(&context)
            .map_err(ComponentCreationError::terminal)?;
        let component = BootstrapProviderComponent::with_descriptor(
            context.specification.descriptor.clone(),
            sessions,
        )
        .map_err(ComponentCreationError::terminal)?
        .with_description(description);
        Ok(ConstructedComponent::bootstrap_provider(Box::new(
            component,
        )))
    }
}

fn scope() -> drasi_computation_plugin_sdk::Scope {
    drasi_computation_plugin_sdk::Scope {
        instance_id: INSTANCE.into(),
        graph_id: ComponentBatch::GRAPH_ID.into(),
        generation: 0,
    }
}

fn resource(name: &str) -> Result<ResourceId> {
    Ok(ResourceId::try_new(name)?)
}

fn provide(
    builder: ComponentBatchBuilder,
    name: &str,
    role: ResourceRole,
    ownership: ResourceOwnership,
    value: ResourceHandle,
) -> Result<ComponentBatchBuilder> {
    let id = resource(name)?;
    Ok(builder
        .declare_resource(ResourceSpecification {
            id: id.clone(),
            role,
            ownership,
            binding: name.into(),
        })?
        .provide_resource(id, value)?)
}

async fn graph(
    plugins: &Plugins,
    path: &Path,
    indexes: &Path,
    port: u16,
    shared: bool,
) -> Result<Graph> {
    graph_with_options(plugins, path, indexes, port, Options::new(shared)).await
}

async fn graph_with_options(
    plugins: &Plugins,
    path: &Path,
    indexes: &Path,
    port: u16,
    options: Options,
) -> Result<Graph> {
    let shared = options.shared;
    let names = options.names;
    let owners = names
        .iter()
        .map(|name| {
            Ok(Arc::new(QuerySourceProgress::new(
                ComponentBatch::GRAPH_ID,
                id(name),
            )?))
        })
        .collect::<Result<Vec<_>>>()?;
    let factory = plugins
        .source
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == if shared {
                    drasi_computation_source_sqlite::SQLITE_SHARED
                } else {
                    drasi_computation_source_sqlite::SQLITE_REPLAY
                }
        })
        .cloned()
        .context("coordinated SQLite source factory")?;
    let bootstrap = plugins
        .bootstrap
        .bootstrap_factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == if shared {
                    drasi_computation_bootstrap_sqlite::SHARED_BOOTSTRAP
                } else {
                    drasi_computation_bootstrap_sqlite::COORDINATED_BOOTSTRAP
                }
        })
        .context("paired SQLite bootstrap factory")?;
    let mut settings = settings(path);
    settings.output = SqliteOutput::Transactions;
    settings.output_capacity = size(1);
    settings.transactions.max_bytes = size(options.max_bytes);
    settings.replay = Some(SqliteReplayConfig {
        max_transactions: size(16),
        max_bytes: size(8 << 20),
    });
    let configuration = super::flat(&settings, Some(port), true);
    let mut builder = ComponentBatch::builder();
    for (name, owner) in names.iter().zip(&owners) {
        builder = provide(
            builder,
            &format!("{name}-progress"),
            ResourceRole::Checkpoint,
            ResourceOwnership::Borrowed,
            ResourceHandle::new(
                ResourceRole::Checkpoint,
                Arc::new(QuerySourceProgressResource(owner.clone())),
            )
            .with_capabilities(ResourceCapabilities::DURABLE),
        )?;
    }
    let progress_id = if shared {
        let group = Arc::new(ConsumerProgressSet::new(
            ComponentBatch::GRAPH_ID,
            id("source"),
            owners.clone(),
        )?);
        builder = provide(
            builder,
            "consumers",
            ResourceRole::Checkpoint,
            ResourceOwnership::Borrowed,
            ResourceHandle::new(ResourceRole::Checkpoint, group.clone())
                .with_capabilities(ResourceCapabilities::DURABLE),
        )?;
        "consumers".to_owned()
    } else {
        "first-progress".to_owned()
    };
    let mut source = factory.specification(id("source"), configuration)?;
    source.dependencies = BTreeMap::from([(
        if shared {
            "consumer_progress".into()
        } else {
            "source_progress".into()
        },
        vec![resource(&progress_id)?],
    )]);
    let superseded = Superseded::default();
    let provider = Arc::new(PairedProviderNode {
        node: NativeBootstrapNodeFactory::new(bootstrap.clone()),
        bootstrap: bootstrap.clone(),
        gates: options
            .gates
            .iter()
            .flat_map(|gates| names.iter().map(|name| id(name)).zip(gates.iter().cloned()))
            .collect(),
        supersede_stopped: options.supersede_stopped,
        superseded: superseded.clone(),
    });
    let provider_spec = provider.specification_from_config(id("provider"), json!({}))?;
    builder = builder
        .component(
            drasi_lib::computation::v1::ComponentName::try_new("provider")
                .expect("fixture component name"),
            provider_spec,
            provider,
        )
        .connect(
            EdgeDefinition::new(
                endpoint("provider", "snapshot"),
                endpoint("source", "bootstrap"),
            ),
            Box::new(BoundedPipeConfig { capacity: 8 }),
        );
    builder = builder
        .component(
            drasi_lib::computation::v1::ComponentName::try_new("source")
                .expect("fixture component name"),
            source,
            factory,
        )
        .bind_stream(endpoint("source", "out"), StreamId::try_new("sqlite/out")?);
    builder = provide(
        builder,
        "catalog",
        ResourceRole::QueryCatalog,
        ResourceOwnership::Graph,
        ResourceHandle::new(
            ResourceRole::QueryCatalog,
            Arc::new(QueryResultsCatalog::new(ComponentBatch::GRAPH_ID)?),
        ),
    )?;
    let factories = FactoryRegistry::standard();
    let query_factory = factories
        .get(
            &ContinuousQueryFactory::default()
                .descriptor()
                .implementation,
        )
        .context("reference query factory")?
        .clone();
    let outlet_factory = factories
        .get(
            &QueryResultsOutletFactory::default()
                .descriptor()
                .implementation,
        )
        .context("query results outlet factory")?
        .clone();
    for name in &names {
        let indexes_id = format!("{name}-indexes");
        let indexes: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
            indexes.join(name),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
            ),
        ));
        builder = provide(
            builder,
            &indexes_id,
            ResourceRole::IndexBackend,
            ResourceOwnership::Borrowed,
            drasi_lib::computation::v1::QueryIndexProviderResource::handle(
                ResourceRole::IndexBackend,
                indexes,
            ),
        )?;
        let mut query = query_factory.specification_from_config(id(name), json!({
            "query":"MATCH (n:items) RETURN n.id AS id,n.value AS value",
            "stream":format!("{name}/out"), "outbox_capacity":32,
            "execution":QueryExecutionSettings { source_transactions: Some(settings.transactions), ..Default::default() },
            "recovery":match options.recovery { QueryRecoveryPolicy::Strict => "strict", QueryRecoveryPolicy::AutoReset => "auto_reset" },
        }))?;
        query.dependencies = BTreeMap::from([
            ("indexes".into(), vec![resource(&indexes_id)?]),
            ("catalog".into(), vec![resource("catalog")?]),
            (
                "source_progress".into(),
                vec![resource(&format!("{name}-progress"))?],
            ),
        ]);
        let output = format!("{name}-results");
        let sink = ComponentSpecification {
            descriptor: QueryResultsOutlet::new(
                id(&output),
                QueryResultsCatalog::new(ComponentBatch::GRAPH_ID)?,
            )
            .descriptor()
            .clone(),
            role: ComponentRole::Sink,
            completion: Some(SinkCompletion::Handled),
            implementation: outlet_factory.descriptor().implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::new(),
            dependencies: BTreeMap::from([("catalog".into(), vec![resource("catalog")?])]),
        };
        builder = builder
            .component(
                drasi_lib::computation::v1::ComponentName::try_new(*name)
                    .expect("fixture component name"),
                query,
                query_factory.clone(),
            )
            .component(
                drasi_lib::computation::v1::ComponentName::try_new(output.as_str())
                    .expect("fixture component name"),
                sink,
                outlet_factory.clone(),
            )
            .bind_stream(
                endpoint(name, "out"),
                StreamId::try_new(format!("{name}/out"))?,
            )
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("source", "snapshot"), endpoint(name, "bootstrap")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint(&output, "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            );
    }
    let core = Arc::new(
        DrasiLib::builder()
            .with_allow_external_component_ids(true)
            .with_query_factory(
                drasi_lib::computation::v1::FactoryRegistry::standard()
                    .get(
                        &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            // Parallel tests run graphs with identical instance, graph and
            // component IDs in one process; each source coordinator is found
            // only from its own running graph.
            .with_id(INSTANCE)
            .with_components(builder.build()?)
            .build()
            .await?,
    );
    Ok(Graph {
        core,
        owners,
        names,
        superseded,
    })
}

async fn wait_rows(graph: &Graph, expected: &[Value]) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let mut complete = true;
            for name in &graph.names {
                let name = drasi_core_types::ComponentName::try_new(*name)
                    .expect("fixture component name");
                let mut actual = graph.core.get_query_results_by_name(&name).await?;
                actual.sort_by_key(|row| row["id"].as_i64());
                complete &= actual == expected;
            }
            if complete {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("coordinated SQLite results did not converge")?
}

async fn snapshot_live_restart() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = plugins(dynamic)?;
        for shared in [false, true] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("source.db");
            rusqlite::Connection::open(&path)?.execute_batch(
                "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT NOT NULL);
                 INSERT INTO items VALUES(1,'initial'),(2,'before');",
            )?;
            let port = port()?;
            let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
            let opened = graph(
                &plugins,
                &path,
                &directory.path().join("indexes"),
                port,
                shared,
            )
            .await?;
            let result = async {
                opened.core.start().await?;
                tokio::time::timeout(
                    DEADLINE,
                    opened
                        .core
                        .component_by_name(
                            &drasi_core_types::ComponentName::try_new("source")
                                .expect("fixture component name"),
                        )?
                        .wait_started(),
                )
                .await
                .with_context(|| {
                    format!(
                        "snapshot source startup (dynamic={dynamic}, shared={shared}): {:?}",
                        opened
                            .core
                            .computation_control()
                            .map(|control| control.observed())
                    )
                })??;
                wait_rows(
                    &opened,
                    &[
                        json!({"id":1,"value":"initial"}),
                        json!({"id":2,"value":"before"}),
                    ],
                )
                .await?;
                for owner in &opened.owners {
                    let state = owner.snapshot();
                    anyhow::ensure!(
                        state.persistent && state.bootstrap_complete,
                        "paired initialization is not committed"
                    );
                    let watermark = state
                        .checkpoints
                        .get(&SourceProgressKey::Source(
                            drasi_core_types::ComponentId::try_new(
                                "00000000-0000-0000-0000-00000000004d",
                            )
                            .expect("fixture UUID"),
                        ))
                        .context("source watermark")?;
                    anyhow::ensure!(
                        watermark.sequence == 0 && watermark.source_position.is_some(),
                        "initial snapshot has no real journal boundary"
                    );
                }
                let url = format!("http://127.0.0.1:{port}/api/batch");
                client
                    .post(url)
                    .json(&json!({"operations":[
                        {"op":"update","table":"items","id":"1","data":{"id":10,"value":"rekey"}},
                        {"op":"delete","table":"items","id":"2"},
                        {"op":"insert","table":"items","data":{"id":3,"value":"live"}}
                    ]}))
                    .send()
                    .await?
                    .error_for_status()?;
                wait_rows(
                    &opened,
                    &[
                        json!({"id":3,"value":"live"}),
                        json!({"id":10,"value":"rekey"}),
                    ],
                )
                .await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let cleanup = opened.core.shutdown().await;
            result?;
            cleanup?;
            drop(opened);
            let restarted = graph(
                &plugins,
                &path,
                &directory.path().join("indexes"),
                port,
                shared,
            )
            .await?;
            let result = async {
                restarted.core.start().await?;
                tokio::time::timeout(
                    DEADLINE,
                    restarted
                        .core
                        .component_by_name(
                            &drasi_core_types::ComponentName::try_new("source")
                                .expect("fixture component name"),
                        )?
                        .wait_started(),
                )
                .await
                .with_context(|| {
                    format!(
                    "snapshot restarted source startup (dynamic={dynamic}, shared={shared}): {:?}",
                    restarted.core.computation_control().map(|control| control.observed())
                )
                })??;
                wait_rows(
                    &restarted,
                    &[
                        json!({"id":3,"value":"live"}),
                        json!({"id":10,"value":"rekey"}),
                    ],
                )
                .await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let cleanup = restarted.core.shutdown().await;
            result?;
            cleanup?;
            drop(restarted);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn paired_snapshot_current_thread_static_and_dynamic() -> Result<()> {
    snapshot_live_restart().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paired_snapshot_multi_thread_static_and_dynamic() -> Result<()> {
    snapshot_live_restart().await
}

async fn binding_identity_and_retirement() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = plugins(dynamic)?;
        for shared in [false, true] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("must-not-open.db");
            let mut config = settings(&path);
            config.output = SqliteOutput::Transactions;
            config.replay = Some(SqliteReplayConfig {
                max_transactions: size(8),
                max_bytes: size(4 << 20),
            });
            let configuration = super::flat(&config, None, true);
            let owner = Arc::new(QuerySourceProgress::new(
                ComponentBatch::GRAPH_ID,
                id("first"),
            )?);
            let group = Arc::new(ConsumerProgressSet::new(
                ComponentBatch::GRAPH_ID,
                id("source"),
                [owner.clone()],
            )?);
            let binding = || {
                if shared {
                    NativeSourceBootstrapProgress::Shared(group.clone())
                } else {
                    NativeSourceBootstrapProgress::Single(owner.clone())
                }
            };
            let factory = plugins
                .source
                .factories()
                .iter()
                .find(|factory| {
                    factory.metadata().implementation.name.as_ref()
                        == if shared {
                            drasi_computation_source_sqlite::SQLITE_SHARED
                        } else {
                            drasi_computation_source_sqlite::SQLITE_REPLAY
                        }
                })
                .context("source factory")?;
            let bootstrap = plugins
                .bootstrap
                .bootstrap_factories()
                .iter()
                .find(|factory| {
                    factory.metadata().implementation.name.as_ref()
                        == if shared {
                            drasi_computation_bootstrap_sqlite::SHARED_BOOTSTRAP
                        } else {
                            drasi_computation_bootstrap_sqlite::COORDINATED_BOOTSTRAP
                        }
                })
                .context("bootstrap factory")?;
            let coordinator = factory.create_source_bootstrap(
                id("source"),
                configuration.clone(),
                scope(),
                binding(),
            )?;
            anyhow::ensure!(
                bootstrap
                    .create_provider(
                        id("first"),
                        json!({}),
                        Some(scope()),
                        Some(owner.clone()),
                        drasi_computation_plugin_sdk::BootstrapSession::default()
                    )
                    .is_err(),
                "paired factory accepted no coordinator"
            );
            let substitute = Arc::new(QuerySourceProgress::new(
                ComponentBatch::GRAPH_ID,
                id("first"),
            )?);
            anyhow::ensure!(
                bootstrap
                    .create_paired_query_provider(
                        id("first"),
                        json!({}),
                        scope(),
                        substitute,
                        coordinator.clone(),
                        None
                    )
                    .is_err(),
                "same-name query substituted for actual owner"
            );
            for configuration in [
                json!({"source":id("second"),"stream":"sqlite/out"}),
                json!({"source":id("source"),"stream":"different"}),
            ] {
                anyhow::ensure!(
                    bootstrap
                        .create_paired_query_provider(
                            id("first"),
                            configuration,
                            scope(),
                            owner.clone(),
                            coordinator.clone(),
                            None
                        )
                        .is_err(),
                    "a paired provider takes no source or stream configuration"
                );
            }
            let provider = bootstrap.create_paired_query_provider(
                id("first"),
                json!({}),
                scope(),
                owner.clone(),
                coordinator.clone(),
                None,
            )?;
            anyhow::ensure!(
                bootstrap
                    .create_paired_query_provider(
                        id("first"),
                        json!({}),
                        scope(),
                        owner.clone(),
                        coordinator.clone(),
                        None
                    )
                    .is_err(),
                "duplicate paired provider acquired the same member"
            );
            let substitute = if shared {
                NativeSourceBootstrapProgress::Shared(Arc::new(ConsumerProgressSet::new(
                    ComponentBatch::GRAPH_ID,
                    id("source"),
                    [owner.clone()],
                )?))
            } else {
                NativeSourceBootstrapProgress::Single(Arc::new(QuerySourceProgress::new(
                    ComponentBatch::GRAPH_ID,
                    id("first"),
                )?))
            };
            anyhow::ensure!(
                factory
                    .create_component_with_source_bootstrap(
                        id("source"),
                        configuration.clone(),
                        scope(),
                        coordinator.clone(),
                        substitute,
                    )
                    .is_err(),
                "same-named live progress substituted for the captured actual owner"
            );
            let mut foreign_scope = scope();
            foreign_scope.instance_id = "different-instance".into();
            anyhow::ensure!(
                factory
                    .create_component_with_source_bootstrap(
                        id("source"),
                        configuration.clone(),
                        foreign_scope,
                        coordinator.clone(),
                        binding(),
                    )
                    .is_err(),
                "source reused a coordinator from another instance"
            );
            let mut source_scope = scope();
            source_scope.generation = 9;
            let source = factory.create_component_with_source_bootstrap(
                id("source"),
                configuration.clone(),
                source_scope,
                coordinator.clone(),
                binding(),
            )?;
            anyhow::ensure!(
                factory
                    .create_component_with_source_bootstrap(
                        id("source"),
                        configuration.clone(),
                        scope(),
                        coordinator.clone(),
                        binding()
                    )
                    .is_err(),
                "duplicate live source owner acquired the coordinator"
            );
            drop(source);
            let mut changed = configuration.clone();
            changed["maxSqlBytes"] = json!(32768);
            anyhow::ensure!(
                factory
                    .create_component_with_source_bootstrap(
                        id("source"),
                        changed,
                        scope(),
                        coordinator.clone(),
                        binding()
                    )
                    .is_err(),
                "coordinator accepted a different immutable configuration"
            );
            let mut source_scope = scope();
            source_scope.generation = 10;
            let replacement = factory.create_component_with_source_bootstrap(
                id("source"),
                configuration.clone(),
                source_scope,
                coordinator.clone(),
                binding(),
            )?;
            drop(replacement);
            anyhow::ensure!(!path.exists(), "coordinator construction opened SQLite");
            drop(provider);
            anyhow::ensure!(
                bootstrap
                    .create_paired_query_provider(
                        id("first"),
                        json!({}),
                        scope(),
                        owner.clone(),
                        coordinator.clone(),
                        None,
                    )
                    .is_err(),
                "dropping an unstopped provider erased its cleanup obligation"
            );
            coordinator.shutdown().await?;
            coordinator.shutdown().await?;
            anyhow::ensure!(
                factory
                    .create_component_with_source_bootstrap(
                        id("source"),
                        configuration,
                        scope(),
                        coordinator.clone(),
                        binding()
                    )
                    .is_err(),
                "retired coordinator accepted a new source"
            );
            anyhow::ensure!(
                bootstrap
                    .create_paired_query_provider(
                        id("first"),
                        json!({}),
                        scope(),
                        owner,
                        coordinator.clone(),
                        None
                    )
                    .is_err(),
                "retired coordinator accepted a new provider"
            );
            anyhow::ensure!(!path.exists(), "retirement created source data");
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn paired_binding_current_thread_static_and_dynamic() -> Result<()> {
    binding_identity_and_retirement().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paired_binding_multi_thread_static_and_dynamic() -> Result<()> {
    binding_identity_and_retirement().await
}

fn gates(count: usize) -> Vec<Arc<SnapshotGate>> {
    (0..count)
        .map(|_| {
            Arc::new(SnapshotGate {
                permits: Arc::new(tokio::sync::Semaphore::new(1)),
                requests: AtomicUsize::new(0),
            })
        })
        .collect()
}

/// Waits until a query holds a partial initialization and returns its index.
async fn wait_partial(gates: &[Arc<SnapshotGate>]) -> Result<usize> {
    tokio::time::timeout(DEADLINE, async {
        // The query asks for its second envelope only after committing the first.
        loop {
            if let Some(index) = gates
                .iter()
                .position(|gate| gate.requests.load(Ordering::SeqCst) >= 2)
            {
                return index;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .context("real paired snapshot did not commit its first envelope")
}

async fn retained_provider_replacement() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = plugins(dynamic)?;
        for shared in [false, true] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("source.db");
            rusqlite::Connection::open(&path)?.execute_batch(
                "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT NOT NULL);
                 INSERT INTO items VALUES(1,'first'),(2,'second'),(3,'third');",
            )?;
            let mut options = Options::new(shared);
            let gates = gates(options.names.len());
            options.gates = Some(gates.clone());
            options.supersede_stopped = true;
            let graph = graph_with_options(
                &plugins,
                &path,
                &directory.path().join("indexes"),
                port()?,
                options,
            )
            .await?;
            let result = async {
                let exercise = async {
                    wait_partial(&gates).await?;
                    let superseded = graph
                        .superseded
                        .lock()
                        .map_err(|_| anyhow::anyhow!("superseded providers poisoned"))?
                        .clone();
                    for old in &superseded {
                        let error = match old.prepare().await {
                            Ok(_) => {
                                anyhow::bail!(
                                    "retired provider restarted while replacement was active"
                                )
                            }
                            Err(error) => error,
                        };
                        anyhow::ensure!(
                            format!("{error:#}").contains("paired bootstrap provider was replaced"),
                            "old provider failed for an unrelated reason: {error:#}"
                        );
                        anyhow::ensure!(
                            old.snapshot().await.is_err(),
                            "retired provider reopened a snapshot"
                        );
                        anyhow::ensure!(
                            old.complete_snapshot().await.is_err(),
                            "retired provider completed replacement work"
                        );
                        old.stop().await?;
                    }
                    for gate in &gates {
                        gate.permits.add_permits(8);
                    }
                    Ok::<_, anyhow::Error>(())
                };
                tokio::time::timeout(DEADLINE, async {
                    tokio::try_join!(
                        async { graph.core.start().await.map_err(anyhow::Error::from) },
                        exercise,
                    )
                })
                .await??;
                tokio::time::timeout(
                    DEADLINE,
                    graph
                        .core
                        .component_by_name(
                            &drasi_core_types::ComponentName::try_new("source")
                                .expect("fixture component name"),
                        )?
                        .wait_started(),
                )
                .await??;
                wait_rows(
                    &graph,
                    &[
                        json!({"id":1,"value":"first"}),
                        json!({"id":2,"value":"second"}),
                        json!({"id":3,"value":"third"}),
                    ],
                )
                .await?;
                anyhow::ensure!(
                    graph
                        .owners
                        .iter()
                        .all(|owner| owner.snapshot().bootstrap_complete),
                    "retained old handle interrupted replacement completion"
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
            for gate in &gates {
                gate.permits.add_permits(8);
            }
            let cleanup = graph.core.shutdown().await;
            result?;
            cleanup?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn paired_retained_provider_current_thread_static_and_dynamic() -> Result<()> {
    retained_provider_replacement().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paired_retained_provider_multi_thread_static_and_dynamic() -> Result<()> {
    retained_provider_replacement().await
}

/// Expects startup to fail and the query at `partial`, whose initialization
/// is incomplete, not to publish a completed bootstrap.
async fn expect_failed_initialization(graph: &Graph, partial: usize) -> Result<()> {
    let started = graph.core.start().await;
    let failure: anyhow::Error = match started {
        Err(error) => error.into(),
        Ok(()) => graph
            .core
            .component_by_name(
                &drasi_core_types::ComponentName::try_new("first").expect("fixture component name"),
            )?
            .wait_started()
            .await
            .err()
            .context("incomplete or oversized initialization was accepted")?
            .into(),
    };
    anyhow::ensure!(
        format!("{failure:#}").contains("bootstrap") || format!("{failure:#}").contains("Error"),
        "unexpected startup failure: {failure:#}"
    );
    anyhow::ensure!(
        !graph.owners[partial].snapshot().bootstrap_complete,
        "failed initialization published a completed bootstrap"
    );
    Ok(())
}

async fn incomplete_initialization(cancel: bool) -> Result<()> {
    for dynamic in [false, true] {
        let plugins = plugins(dynamic)?;
        for shared in [false, true] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("source.db");
            let indexes = directory.path().join("indexes");
            let database = rusqlite::Connection::open(&path)?;
            database
                .execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT NOT NULL);")?;
            let value = if cancel {
                "snapshot".to_owned()
            } else {
                "x".repeat(8192)
            };
            for id in 1..=3 {
                database.execute(
                    "INSERT INTO items VALUES(?1,?2)",
                    rusqlite::params![id, value],
                )?;
            }
            drop(database);
            let mut options = Options::new(shared);
            if !cancel {
                options.max_bytes = 1024;
            }
            let gates = gates(options.names.len());
            if cancel {
                options.gates = Some(gates.clone());
            }
            let opened = graph_with_options(&plugins, &path, &indexes, port()?, options).await?;
            let mut partial = 0;
            let result = async {
                if cancel {
                    let core = opened.core.clone();
                    let starting = tokio::spawn(async move { core.start().await });
                    let result = wait_partial(&gates).await.map(|index| partial = index);
                    starting.abort();
                    match starting.await {
                        Ok(result) => result?,
                        Err(error) if error.is_cancelled() => {}
                        Err(error) => return Err(error.into()),
                    }
                    result
                } else {
                    tokio::time::timeout(DEADLINE, expect_failed_initialization(&opened, 0)).await?
                }
            }
            .await;
            let cleanup = opened.core.shutdown().await;
            result?;
            cleanup?;
            drop(opened);
            let mut options = Options::new(shared);
            if !cancel {
                options.max_bytes = 1024;
            }
            let strict = graph_with_options(&plugins, &path, &indexes, port()?, options).await?;
            let result =
                tokio::time::timeout(DEADLINE, expect_failed_initialization(&strict, partial))
                    .await
                    .with_context(|| format!(
                        "strict incomplete startup (dynamic={dynamic}, shared={shared}, cancel={cancel}): {:?}",
                        strict.core.computation_control().map(|control| control.observed())
                    ));
            let cleanup = strict.core.shutdown().await;
            result??;
            cleanup?;
            drop(strict);
            let mut options = Options::new(shared);
            options.recovery = QueryRecoveryPolicy::AutoReset;
            let recovered = graph_with_options(&plugins, &path, &indexes, port()?, options).await?;
            let result = async {
                recovered.core.start().await?;
                tokio::time::timeout(
                    DEADLINE,
                    recovered
                        .core
                        .component_by_name(
                            &drasi_core_types::ComponentName::try_new("source")
                                .expect("fixture component name"),
                        )?
                        .wait_started(),
                )
                .await
                .with_context(|| format!(
                    "reconstructed source startup (dynamic={dynamic}, shared={shared}, cancel={cancel}): {:?}",
                    recovered.core.computation_control().map(|control| control.observed())
                ))??;
                let expected = (1..=3)
                    .map(|id| json!({"id":id,"value":value}))
                    .collect::<Vec<_>>();
                wait_rows(&recovered, &expected).await?;
                anyhow::ensure!(
                    recovered
                        .owners
                        .iter()
                        .all(|owner| owner.snapshot().bootstrap_complete),
                    "explicit reconstruction did not complete real consumer state"
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let cleanup = recovered.core.shutdown().await;
            result?;
            cleanup?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn paired_failed_initialization_current_thread_static_and_dynamic() -> Result<()> {
    incomplete_initialization(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paired_failed_initialization_multi_thread_static_and_dynamic() -> Result<()> {
    incomplete_initialization(false).await
}

#[tokio::test(flavor = "current_thread")]
async fn paired_cancelled_initialization_current_thread_static_and_dynamic() -> Result<()> {
    incomplete_initialization(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paired_cancelled_initialization_multi_thread_static_and_dynamic() -> Result<()> {
    incomplete_initialization(true).await
}

async fn enroll_and_remove_members() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = plugins(dynamic)?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("source.db");
        let indexes = directory.path().join("indexes");
        rusqlite::Connection::open(&path)?.execute_batch(
            "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT NOT NULL);
             INSERT INTO items VALUES(0,'snapshot');",
        )?;
        let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
        let mut expected = vec![json!({"id":0,"value":"snapshot"})];
        for (round, names) in [
            vec!["first", "second"],
            vec!["first", "second", "third"],
            vec!["second", "third"],
        ]
        .into_iter()
        .enumerate()
        {
            let mut options = Options::new(true);
            options.names = names;
            let port = port()?;
            let opened = graph_with_options(&plugins, &path, &indexes, port, options).await?;
            let result = async {
                opened.core.start().await?;
                tokio::time::timeout(
                    DEADLINE,
                    opened
                        .core
                        .component_by_name(
                            &drasi_core_types::ComponentName::try_new("source")
                                .expect("fixture component name"),
                        )?
                        .wait_started(),
                )
                .await
                .with_context(|| {
                    format!(
                        "membership source did not start (dynamic={dynamic}, round={round}): {:?}",
                        opened
                            .core
                            .computation_control()
                            .map(|control| control.observed())
                    )
                })??;
                wait_rows(&opened, &expected).await?;
                let row = json!({"id":round + 1,"value":format!("round-{round}")});
                client
                    .post(format!("http://127.0.0.1:{port}/api/tables/items"))
                    .json(&row)
                    .send()
                    .await?
                    .error_for_status()?;
                expected.push(row);
                wait_rows(&opened, &expected).await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let cleanup = opened.core.shutdown().await;
            result?;
            cleanup?;
            drop(opened);
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn paired_membership_current_thread_static_and_dynamic() -> Result<()> {
    enroll_and_remove_members().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paired_membership_multi_thread_static_and_dynamic() -> Result<()> {
    enroll_and_remove_members().await
}
