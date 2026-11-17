// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native bootstrap factories wired as graph provider nodes.

use super::*;
use drasi_core::interface::StorageDurability;

const QUERY: &str = "6ddf3eb2-187f-5740-a6ba-a07f98257ca8";
const PROVIDER: &str = "1d4f4a3d-6cce-5052-b017-09f1341b67af";
const SINK: &str = "28c0b5df-4504-5437-b597-468cb73ff237";
const SOURCE: &str = "f0d98da5-9a87-5a60-adc0-9dc6bf36a2d2";

fn graph_id() -> &'static str {
    "native-bootstrap"
}

fn progress() -> Arc<QuerySourceProgress> {
    Arc::new(QuerySourceProgress::new(graph_id(), ComponentId::try_new(QUERY).unwrap()).unwrap())
}

struct Sink {
    descriptor: ComponentDescriptor,
}

#[async_trait]
impl ComputationComponent for Sink {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _input: InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}

fn sink() -> anyhow::Result<Sink> {
    Ok(Sink {
        descriptor: ComponentDescriptor::try_new(
            ComponentId::try_new(SINK)?,
            vec![PortDescriptor::new(
                PortId::try_new("in")?,
                PortDirection::Input,
                QueryChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?,
    })
}

/// A live source that emits nothing; the query's data comes from bootstrap.
struct Idle {
    descriptor: ComponentDescriptor,
}

#[async_trait]
impl ComputationComponent for Idle {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSource for Idle {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

fn idle() -> anyhow::Result<Idle> {
    Ok(Idle {
        descriptor: ComponentDescriptor::try_new(
            ComponentId::try_new(SOURCE)?,
            vec![PortDescriptor::new(
                PortId::try_new("out")?,
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?,
    })
}

fn endpoint(component: &str, port: &str) -> Endpoint {
    Endpoint::new(
        ComponentId::try_new(component).unwrap(),
        PortId::try_new(port).unwrap(),
    )
}

async fn query(
    path: &std::path::Path,
    owner: Arc<QuerySourceProgress>,
) -> anyhow::Result<ContinuousQueryTransformer> {
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    ContinuousQueryTransformer::new_with_options(
        ContinuousQueryDefinition {
            graph_id: owner.graph_id().into(),
            id: owner.query_id().clone(),
            query: "MATCH (n:Item) RETURN n.value AS value".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("results")?,
            outbox_capacity: NonZeroUsize::new(8).unwrap(),
        },
        Arc::new(RocksDbComputationProvider::new(
            path,
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
            ),
        )),
        QueryOptions {
            recovery: QueryRecoveryPolicy::AutoReset,
            ..Default::default()
        },
    )
    .await?
    .with_source_progress(owner)
}

/// Runs a provider node wired to a query's bootstrap input until the query
/// has started and returns the query's row count.
pub(crate) async fn run(
    node: Arc<NativeBootstrapNodeFactory>,
    configuration: serde_json::Value,
    path: &std::path::Path,
    owner: Arc<QuerySourceProgress>,
) -> anyhow::Result<usize> {
    let query_id = owner.query_id().to_string();
    let query_id = query_id.as_str();
    let query = query(path, owner.clone()).await?;
    let results = query.results();
    let mut specification =
        node.specification_from_config(ComponentId::try_new(PROVIDER)?, configuration)?;
    let progress_id = ResourceId::try_new("query-progress")?;
    let mut builder = ComputationGraph::builder(owner.graph_id().to_owned());
    if node.bootstrap_factory().metadata().source_progress {
        specification
            .dependencies
            .insert("source_progress".into(), vec![progress_id.clone()]);
        builder = builder
            .declare_resource(ResourceSpecification {
                id: progress_id.clone(),
                role: ResourceRole::Checkpoint,
                ownership: ResourceOwnership::Borrowed,
                binding: "query-progress".into(),
            })?
            .provide_resource(
                progress_id,
                ResourceHandle::new(
                    ResourceRole::Checkpoint,
                    Arc::new(QuerySourceProgressResource(owner.clone())),
                ),
            )?;
    }
    let mut graph = builder
        .component(ComponentName::try_new("provider")?, specification, node)
        .transformer(ComponentName::try_new("query")?, Box::new(query))
        .source(ComponentName::try_new("source")?, Box::new(idle()?))
        .sink(ComponentName::try_new("sink")?, Box::new(sink()?))
        .bind_stream(endpoint(SOURCE, "out"), StreamId::try_new("live")?)
        .bind_stream(endpoint(query_id, "out"), StreamId::try_new("results")?)
        .connect(
            EdgeDefinition::new(endpoint(SOURCE, "out"), endpoint(query_id, "in")),
            Box::new(BoundedPipeConfig { capacity: 16 }),
        )
        .connect(
            EdgeDefinition::new(
                endpoint(PROVIDER, "snapshot"),
                endpoint(query_id, "bootstrap"),
            ),
            Box::new(BoundedPipeConfig { capacity: 16 }),
        )
        .connect(
            EdgeDefinition::new(endpoint(query_id, "out"), endpoint(SINK, "in")),
            Box::new(BoundedPipeConfig { capacity: 16 }),
        )
        .build()?;
    let run = graph.start()?;
    let control = run.control();
    let (result, rows) = tokio::join!(run, async {
        let started = tokio::time::timeout(
            Duration::from_secs(20),
            control
                .component_handle_by_id(&owner.query_id().clone())
                .unwrap()
                .wait_started(),
        )
        .await;
        let rows = results.snapshot().map(|snapshot| snapshot.rows.len());
        control.cancel();
        (started, rows)
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
    rows.0
        .map_err(|_| anyhow::anyhow!("query did not start: {:#?}", control.observed()))??;
    Ok(rows.1?)
}

fn nodes() -> anyhow::Result<Vec<Arc<NativeBootstrapNodeFactory>>> {
    let linked = NativePlugin::from_definition(sdk::PluginDefinition::new_with_bootstrap(
        "fixture/bootstrap-node",
        "1",
        vec![],
        vec![Arc::new(super::bootstrap_fixture::Factory {
            progress: false,
        })],
        vec![GraphChangeCodec::schema()],
    )?)?;
    let dynamic = super::super::factory::recovery_tests::sqlite_plugin()?;
    Ok(vec![
        Arc::new(NativeBootstrapNodeFactory::new(
            linked.bootstrap_factories()[0].clone(),
        )),
        Arc::new(NativeBootstrapNodeFactory::new(
            dynamic.bootstrap_factories()[0].clone(),
        )),
    ])
}

#[tokio::test(flavor = "current_thread")]
async fn static_and_dynamic_provider_nodes_bootstrap_a_query_over_its_edge() -> anyhow::Result<()> {
    for node in nodes()? {
        let directory = tempfile::tempdir()?;
        let rows = run(
            node.clone(),
            json!({"count":2, "durability":StorageDurability::LOCAL_PROCESS_RESTART}),
            directory.path(),
            progress(),
        )
        .await?;
        assert_eq!(
            rows,
            2,
            "{:?}",
            node.bootstrap_factory().metadata().implementation
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn durable_provider_node_does_not_rebootstrap_a_query_with_progress() -> anyhow::Result<()> {
    let plugin = super::super::factory::recovery_tests::sqlite_plugin()?;
    let node = Arc::new(NativeBootstrapNodeFactory::new(
        plugin.bootstrap_factories()[1].clone(),
    ));
    assert!(node.bootstrap_factory().metadata().source_progress);
    let directory = tempfile::tempdir()?;
    let owner = progress();
    for count in [3, 99] {
        let rows = run(
            node.clone(),
            json!({"count":count, "durability":StorageDurability::LOCAL_PROCESS_RESTART}),
            directory.path(),
            owner.clone(),
        )
        .await?;
        assert_eq!(rows, 3, "the restart must reuse the committed snapshot");
        assert!(owner.snapshot().bootstrap_complete);
    }
    Ok(())
}

#[test]
fn provider_nodes_validate_their_progress_dependencies() -> anyhow::Result<()> {
    let plugin = super::super::factory::recovery_tests::sqlite_plugin()?;
    let node = NativeBootstrapNodeFactory::new(plugin.bootstrap_factories()[1].clone());
    assert_eq!(node.descriptor().role, ComponentRole::BootstrapProvider);
    let mut specification = node.specification_from_config(
        ComponentId::try_new(PROVIDER)?,
        json!({"durability":StorageDurability::LOCAL_PROCESS_RESTART}),
    )?;
    assert_eq!(specification.descriptor.ports().len(), 1);
    assert_eq!(
        specification.descriptor.ports()[0].kind(),
        drasi_computation_contracts::PortKind::Bootstrap
    );
    assert!(node.validate(&specification).is_err());
    specification.dependencies.insert(
        "source_progress".into(),
        vec![ResourceId::try_new("progress")?],
    );
    node.validate(&specification)?;
    specification
        .dependencies
        .insert("bootstrap".into(), vec![ResourceId::try_new("other")?]);
    assert!(node.validate(&specification).is_err());
    Ok(())
}
