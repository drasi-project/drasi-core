// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{
    computation::v1::*,
    management::*,
    wal::{WalProvider, WriteAheadLogConfig},
    DrasiLib,
};
use drasi_state_store_redb::{RedbConfigurationStore, RedbStateStoreProvider};
use drasi_wal_redb::RedbWalProvider;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

const DEADLINE: Duration = Duration::from_secs(15);

fn effect_sequences(root: &Path, graph: &str) -> Result<Vec<u64>> {
    let records = std::fs::read_to_string(root.join(format!("{graph}.jsonl")))?;
    records
        .lines()
        .map(|line| {
            let record: Value = serde_json::from_str(line)?;
            let sequence = record["sequence"].as_u64().context("effect sequence")?;
            let diffs: Vec<drasi_lib::channels::ResultDiff> =
                serde_json::from_value(record["diffs"].clone())?;
            let after = match diffs.as_slice() {
                [drasi_lib::channels::ResultDiff::Add { data, .. }] => data,
                [drasi_lib::channels::ResultDiff::Update { after, .. }]
                | [drasi_lib::channels::ResultDiff::Aggregation { after, .. }] => after,
                _ => anyhow::bail!("unexpected aggregate effect {diffs:?}"),
            };
            anyhow::ensure!(
                after == &json!({"total":sequence as f64}),
                "effect does not match committed aggregate"
            );
            Ok(sequence)
        })
        .collect()
}

struct Resources {
    wal: Arc<RedbWalProvider>,
    state: Arc<RedbStateStoreProvider>,
    indexes: Arc<dyn ComputationIndexProvider>,
    catalogs: Mutex<BTreeMap<String, QueryResultsCatalog>>,
    channels: Mutex<BTreeMap<String, Weak<QosChannel>>>,
}

impl Resources {
    fn new(root: &Path) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            wal: Arc::new(RedbWalProvider::new(root.join("wal"))),
            state: Arc::new(RedbStateStoreProvider::new(root.join("consumers.redb"))?),
            indexes: LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(
                root.join("indexes"),
                false,
                false,
            ))),
            catalogs: Mutex::new(BTreeMap::new()),
            channels: Mutex::new(BTreeMap::new()),
        }))
    }
    fn channel(&self, graph: &str, resource: &str) -> Result<Arc<QosChannel>> {
        self.channels
            .lock()
            .expect("channel observations")
            .get(&format!("{graph}/{resource}"))
            .and_then(Weak::upgrade)
            .context("QoS channel")
    }
    fn catalog(&self, graph: &str) -> Result<QueryResultsCatalog> {
        self.catalogs
            .lock()
            .expect("query catalogs")
            .get(graph)
            .cloned()
            .context("query catalog")
    }
    async fn append(&self, graph: &str, id: u64) -> Result<()> {
        let partition = format!("{graph}-input");
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(&partition, &id.to_string()),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: id,
                },
                properties: ElementPropertyMap::from(json!({"value":1})),
            },
        };
        assert_eq!(self.wal.append(&partition, &change).await?, id);
        Ok(())
    }
}

fn channel_definition() -> QosChannelDefinition {
    QosChannelDefinition {
        stream: StreamId::try_new("query/out").expect("stream"),
        capacity: NonZeroUsize::new(2).expect("capacity"),
        durable: true,
        retention: RetentionPolicy::Backpressure,
        subscribers: BTreeMap::from([("reaction".into(), SubscriptionStart::Earliest)]),
    }
}

#[async_trait]
impl ManagementResourceResolver for Resources {
    async fn resolve(
        &self,
        _: &str,
        graph: &str,
        specification: &ResourceSpecification,
        config: &Value,
    ) -> Result<ResourceHandle> {
        let handle = match config["kind"].as_str().context("resource kind")? {
            "wal" => {
                let partition = format!("{graph}-input");
                self.wal
                    .register(&partition, WriteAheadLogConfig::default())
                    .await?;
                ResourceHandle::new(
                    ResourceRole::Wal,
                    Arc::new(WalSourceResource {
                        provider: self.wal.clone(),
                        partition,
                    }),
                )
            }
            "indexes" => ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(QueryIndexProviderResource(self.indexes.clone())),
            ),
            "progress" => ResourceHandle::new(
                ResourceRole::Checkpoint,
                Arc::new(QuerySourceProgressResource(Arc::new(
                    QuerySourceProgress::new(graph, ComponentId::try_new("query")?)?,
                ))),
            ),
            "catalog" => {
                let catalog = QueryResultsCatalog::new(graph)?;
                self.catalogs
                    .lock()
                    .expect("query catalogs")
                    .insert(graph.to_owned(), catalog.clone());
                ResourceHandle::new(ResourceRole::QueryCatalog, Arc::new(catalog))
            }
            "consumer" => ResourceHandle::new(
                ResourceRole::Checkpoint,
                Arc::new(ConsumerProgressResource(Arc::new(
                    StateStoreConsumerProgress::new(graph, "reaction", self.state.clone())?,
                ))),
            ),
            kind @ ("output" | "incoming") => {
                let channel = QosChannel::persistent(
                    if kind == "output" {
                        channel_definition()
                    } else {
                        incoming_definition()
                    },
                    self.indexes.create_indexes(graph, kind).await?,
                    FactoryRegistry::standard()
                        .envelope_codec(NonZeroUsize::new(1024 * 1024).expect("codec limit"))?,
                    kind,
                )
                .await?;
                self.channels
                    .lock()
                    .expect("channel observations")
                    .insert(format!("{graph}/{kind}"), Arc::downgrade(&channel));
                channel.resource()
            }
            kind => anyhow::bail!("unsupported resource recipe {kind}"),
        };
        anyhow::ensure!(
            handle.role() == specification.role,
            "resource role mismatch"
        );
        Ok(handle)
    }
}

struct RecordingReaction {
    descriptor: ComponentDescriptor,
    path: PathBuf,
}

#[async_trait]
impl ComputationComponent for RecordingReaction {
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
impl EnvelopeSink for RecordingReaction {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        let sequence = QueryChangeCodec::query_sequence(&input.envelope)?;
        let legacy = QueryChangeCodec::to_legacy_result(&input.envelope)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(
            &mut file,
            &json!({"sequence":sequence,"diffs":legacy.results}),
        )?;
        writeln!(file)?;
        file.sync_all()?;
        Ok(())
    }
}

struct ReactionFactory(FactoryDescriptor);
impl ReactionFactory {
    fn new() -> Result<Self> {
        Ok(Self(FactoryDescriptor {
            implementation: ImplementationIdentity::try_new("test/checkpointed-reaction", "1")?,
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: ConfigurationSchema {
                fields: BTreeMap::from([(
                    Arc::from("path"),
                    ConfigurationField {
                        value_type: ConfigurationType::String,
                        required: true,
                        secret: false,
                    },
                )]),
                allow_additional: false,
            },
            dependencies: BTreeMap::from([(
                Arc::from("progress"),
                ResourceRequirement::exactly_one::<ConsumerProgressResource>(
                    ResourceRole::Checkpoint,
                ),
            )]),
        }))
    }
}
#[async_trait]
impl ComponentFactory for ReactionFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.0
    }
    fn validate(&self, _: &ComponentSpecification) -> Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let progress = context
            .resources::<ConsumerProgressResource>("progress")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .context("consumer progress")
            .map_err(ComponentCreationError::terminal)?;
        let path = context.configuration()["path"]
            .as_str()
            .context("reaction path")
            .map_err(ComponentCreationError::terminal)?;
        let reaction = CheckpointedSink::new(
            Box::new(RecordingReaction {
                descriptor: context.specification.descriptor.clone(),
                path: PathBuf::from(path),
            }),
            progress.0.clone(),
        )
        .map_err(ComponentCreationError::terminal)?;
        Ok(ConstructedComponent::sink(Box::new(reaction)))
    }
}

fn resource(id: &str) -> ResourceId {
    ResourceId::try_new(id).expect("resource ID")
}
fn endpoint(id: &str, port: &str) -> Result<Endpoint> {
    Ok(Endpoint::new(
        ComponentId::try_new(id)?,
        PortId::try_new(port)?,
    ))
}
fn source_descriptor() -> Result<ComponentDescriptor> {
    Ok(ComponentDescriptor::try_new(
        ComponentId::try_new("source")?,
        vec![PortDescriptor::new(
            PortId::try_new("out")?,
            PortDirection::Output,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )?)
}

fn incoming_definition() -> QosChannelDefinition {
    QosChannelDefinition {
        stream: StreamId::try_new("source/out").expect("stream"),
        subscribers: BTreeMap::from([("query".into(), SubscriptionStart::Earliest)]),
        ..channel_definition()
    }
}

fn desired(root: &Path, durable_input: bool) -> Result<DesiredInstance> {
    let mut graphs = Vec::new();
    for graph in ["left", "right"] {
        let mut topology = ComputationGraph::empty(graph)?
            .snapshot()
            .select(GraphSelection::All)?;
        for (id, role, owned) in [
            ("wal", ResourceRole::Wal, false),
            ("indexes", ResourceRole::IndexBackend, false),
            ("progress", ResourceRole::Checkpoint, false),
            ("catalog", ResourceRole::QueryCatalog, false),
            ("consumer", ResourceRole::Checkpoint, false),
            ("output", ResourceRole::StateStore, true),
        ] {
            topology.resources.push(ResourceSpecification {
                id: resource(id),
                role,
                ownership: if owned {
                    ResourceOwnership::Graph
                } else {
                    ResourceOwnership::Borrowed
                },
                binding: id.into(),
            });
            topology
                .resource_configurations
                .insert(resource(id), json!({"kind":id}));
        }
        if durable_input {
            topology.resources.push(ResourceSpecification {
                id: resource("incoming"),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Graph,
                binding: "incoming".into(),
            });
            topology
                .resource_configurations
                .insert(resource("incoming"), json!({"kind":"incoming"}));
        }
        let source = ComponentSpecification {
            descriptor: source_descriptor()?,
            role: ComponentRole::Source,
            completion: None,
            implementation: WalReplaySourceFactory::default()
                .descriptor()
                .implementation
                .clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([(
                Arc::from("stream"),
                ConfigurationValue::Literal(json!("source/out")),
            )]),
            dependencies: BTreeMap::from([
                (Arc::from("wal"), vec![resource("wal")]),
                (Arc::from("source_progress"), vec![resource("progress")]),
            ]),
        };
        let definition = ContinuousQueryDefinition {
            graph_id: graph.into(),
            id: ComponentId::try_new("query")?,
            query: "MATCH (n:Item) RETURN sum(n.value) AS total".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out")?,
            outbox_capacity: NonZeroUsize::new(16).expect("query outbox"),
        };
        let query = ComponentSpecification {
            descriptor: definition.descriptor(),
            role: ComponentRole::Query,
            completion: None,
            implementation: ContinuousQueryFactory::default()
                .descriptor()
                .implementation
                .clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([
                (
                    Arc::from("query"),
                    ConfigurationValue::Literal(json!(definition.query)),
                ),
                (
                    Arc::from("stream"),
                    ConfigurationValue::Literal(json!("query/out")),
                ),
                (
                    Arc::from("outbox_capacity"),
                    ConfigurationValue::Literal(json!(16)),
                ),
            ]),
            dependencies: BTreeMap::from([
                (Arc::from("indexes"), vec![resource("indexes")]),
                (Arc::from("source_progress"), vec![resource("progress")]),
                (Arc::from("catalog"), vec![resource("catalog")]),
            ]),
        };
        let reaction = ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("reaction")?,
                vec![PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?,
            role: ComponentRole::Sink,
            completion: Some(SinkCompletion::Handled),
            implementation: ReactionFactory::new()?.descriptor().implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([(
                Arc::from("path"),
                ConfigurationValue::Literal(json!(root.join(format!("{graph}.jsonl")))),
            )]),
            dependencies: BTreeMap::from([(Arc::from("progress"), vec![resource("consumer")])]),
        };
        for (spec, stream) in [
            (source, Some("source/out")),
            (query, Some("query/out")),
            (reaction, None),
        ] {
            topology.components.push(DesiredComponent {
                descriptor: spec.descriptor.clone(),
                role: spec.role,
                completion: spec.completion,
                streams: match stream {
                    Some(value) => {
                        BTreeMap::from([(PortId::try_new("out")?, StreamId::try_new(value)?)])
                    }
                    None => BTreeMap::new(),
                },
                lifecycle: LifecyclePolicy::default(),
                input_merge: InputMergePolicy::Arrival,
                construction: ComponentConstruction::Factory(spec),
            });
        }
        topology.relationships = vec![
            DesiredRelationship {
                definition: EdgeDefinition::new(
                    endpoint("source", "out")?,
                    endpoint("query", "in")?,
                ),
                policy: RelationshipPolicy::default(),
                pipe: if durable_input {
                    DesiredPipe::Qos(incoming_definition().pipe(resource("incoming"), "query"))
                } else {
                    DesiredPipe::Bounded { capacity: 2 }
                },
            },
            DesiredRelationship {
                definition: EdgeDefinition::new(
                    endpoint("query", "out")?,
                    endpoint("reaction", "in")?,
                ),
                policy: RelationshipPolicy::default(),
                pipe: DesiredPipe::Qos(channel_definition().pipe(resource("output"), "reaction")),
            },
        ];
        graphs.push(DesiredGraph {
            auto_start: true,
            topology,
        });
    }
    Ok(DesiredInstance { version: 1, graphs })
}

async fn open(root: &Path, reaction_available: bool) -> Result<(DrasiLib, Arc<Resources>)> {
    let resources = Resources::new(root)?;
    let mut factories = FactoryRegistry::standard();
    if reaction_available {
        factories.register(Arc::new(ReactionFactory::new()?))?;
    }
    let core = DrasiLib::builder()
        .with_id("factory-lifecycle")
        .with_configuration_store(Arc::new(RedbConfigurationStore::new(
            root.join("config.redb"),
            [37; 32],
        )?))
        .with_component_factories(factories)
        .with_management_resources(resources.clone())
        .build()
        .await?;
    Ok((core, resources))
}

async fn wait_query(resources: &Resources, graph: &str, sequence: u64) -> Result<()> {
    let catalog = resources.catalog(graph)?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            let snapshot = catalog.snapshot("query", DEADLINE).await?;
            anyhow::ensure!(
                snapshot.as_of_sequence <= sequence,
                "query recomputed committed input"
            );
            if snapshot.as_of_sequence == sequence {
                assert_eq!(snapshot.rows.len(), 1);
                let row = QueryChangeCodec::decode_row(
                    snapshot.rows.values().next().context("aggregate row")?,
                )?;
                assert_eq!(
                    QueryChangeCodec::row_values_to_json(&row.values),
                    json!({"total":sequence as f64})
                );
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}

async fn wait_delivery(resources: &Resources, graph: &str, sequence: u64) -> Result<()> {
    wait_query(resources, graph, sequence).await?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            let channel = resources.channel(graph, "output")?.progress().await?;
            if channel.accepted >= sequence && channel.processed["reaction"] == channel.accepted {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await?
}

async fn stage(core: &DrasiLib, graph: &str, component: &str, start: bool) -> Result<()> {
    let handle = core.get_computation_graph(graph).await?;
    let control = handle.control();
    let selected = GraphSelection::Exact(vec![ComponentId::try_new(component)?]);
    if start {
        let report = tokio::time::timeout(
            DEADLINE,
            control.start_components(control.desired_snapshot().revision, selected),
        )
        .await??;
        assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
    } else {
        let report = tokio::time::timeout(
            DEADLINE,
            control.stop_components(control.desired_snapshot().revision, selected),
        )
        .await??;
        assert_eq!(report.summary, OperationSummary::Completed, "{report:?}");
    }
    Ok(())
}

#[tokio::test]
async fn bulk_factory_graphs_restore_definitions_state_backlog_and_independent_stage_positions(
) -> Result<()> {
    for durable_input in [false, true] {
        tokio::time::timeout(Duration::from_secs(60), factory_lifecycle(durable_input)).await??;
    }
    Ok(())
}

async fn factory_lifecycle(durable_input: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let target = desired(directory.path(), durable_input)?;
    let (core, resources) = open(directory.path(), false).await?;
    core.apply_desired_state(0, "configure-both", target.clone())
        .await?;
    let pending = core.reconcile_desired_state().await?;
    assert!(!pending.converged());
    assert!(
        pending
            .graphs
            .iter()
            .all(|graph| graph.resource_errors.is_empty()),
        "{pending:?}"
    );
    for graph in ["left", "right"] {
        let observed = core.get_computation_graph(graph).await?.observed();
        assert_eq!(observed.components.len(), 3);
        assert!(observed.components[&ComponentId::try_new("reaction")?]
            .failure
            .as_ref()
            .is_some_and(|failure| failure.cause.to_string().contains("not registered")));
    }
    core.register_component_factory(Arc::new(ReactionFactory::new()?))
        .await?;
    let realized = core.reconcile_desired_state().await?;
    assert!(
        realized.converged(),
        "{realized:?}; left={:?}",
        core.get_computation_graph("left").await?.observed()
    );
    let configuration = core.desired_configuration()?;
    core.snapshot_desired_configuration("configured").await?;
    core.start().await?;
    for graph in ["left", "right"] {
        resources.append(graph, 1).await?;
        wait_delivery(&resources, graph, 1).await?;
    }
    stage(&core, "left", "source", false).await?;
    resources.append("left", 2).await?;
    resources.append("right", 2).await?;
    wait_delivery(&resources, "right", 2).await?;
    assert_eq!(
        resources
            .catalog("left")?
            .snapshot("query", DEADLINE)
            .await?
            .as_of_sequence,
        1
    );
    stage(&core, "left", "source", true).await?;
    wait_delivery(&resources, "left", 2).await?;
    stage(&core, "left", "query", false).await?;
    resources.append("left", 3).await?;
    stage(&core, "left", "query", true).await?;
    wait_delivery(&resources, "left", 3).await?;
    stage(&core, "left", "reaction", false).await?;
    resources.append("left", 4).await?;
    resources.append("left", 5).await?;
    wait_query(&resources, "left", 5).await?;
    assert_eq!(
        resources
            .channel("left", "output")?
            .progress()
            .await?
            .processed["reaction"],
        3
    );
    resources.append("left", 6).await?;
    wait_query(&resources, "left", 6).await?;
    tokio::time::timeout(DEADLINE, core.shutdown()).await??;
    drop((core, resources));

    let (restored, resources) = open(directory.path(), true).await?;
    assert_eq!(restored.desired_configuration()?, configuration);
    assert_eq!(
        restored.load_configuration_snapshot("configured").await?,
        Some(configuration)
    );
    restored.start().await?;
    let left = restored.get_computation_graph("left").await?;
    wait_delivery(&resources, "left", 6)
        .await
        .with_context(|| format!("restore pending left output: {:?}", left.observed()))?;
    wait_delivery(&resources, "right", 2).await?;
    resources.append("left", 7).await?;
    resources.append("right", 3).await?;
    wait_delivery(&resources, "left", 7)
        .await
        .with_context(|| format!("resume fresh left input: {:?}", left.observed()))?;
    wait_delivery(&resources, "right", 3).await?;
    restored.shutdown().await?;
    for (graph, count) in [("left", 7), ("right", 3)] {
        assert_eq!(
            effect_sequences(directory.path(), graph)?,
            (1..=count).collect::<Vec<_>>(),
            "{graph} effects"
        );
    }
    Ok(())
}

#[tokio::test]
async fn factory_restart_replays_input_accepted_while_the_query_was_stopped() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (core, resources) = open(directory.path(), true).await?;
    core.apply_desired_state(0, "initial", desired(directory.path(), true)?)
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    core.start().await?;
    resources.append("left", 1).await?;
    wait_delivery(&resources, "left", 1).await?;
    stage(&core, "left", "query", false).await?;
    resources.append("left", 2).await?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            let progress = resources.channel("left", "incoming")?.progress().await?;
            if progress.accepted == 2 {
                assert_eq!(progress.processed["query"], 1);
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    core.shutdown().await?;
    drop((core, resources));
    let (core, resources) = open(directory.path(), true).await?;
    core.start().await?;
    let left = core.get_computation_graph("left").await?;
    wait_delivery(&resources, "left", 2)
        .await
        .with_context(|| format!("accepted input recovery: {:?}", left.observed()))?;
    resources.append("left", 3).await?;
    wait_delivery(&resources, "left", 3)
        .await
        .with_context(|| format!("fresh input after recovery: {:?}", left.observed()))?;
    core.shutdown().await?;
    assert_eq!(effect_sequences(directory.path(), "left")?, [1, 2, 3]);
    Ok(())
}

#[tokio::test]
async fn source_restart_with_pipe_ahead_preserves_transport_identity_on_later_reconstruction(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (core, resources) = open(directory.path(), true).await?;
    core.apply_desired_state(0, "initial", desired(directory.path(), true)?)
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    core.start().await?;
    resources.append("left", 1).await?;
    wait_delivery(&resources, "left", 1).await?;
    stage(&core, "left", "reaction", false).await?;
    for sequence in 2..=4 {
        resources.append("left", sequence).await?;
    }
    wait_query(&resources, "left", 4).await?;
    resources.append("left", 5).await?;
    tokio::time::timeout(DEADLINE, async {
        while resources
            .channel("left", "incoming")?
            .progress()
            .await?
            .accepted
            != 5
        {
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    stage(&core, "left", "source", false).await?;
    stage(&core, "left", "source", true).await?;
    stage(&core, "left", "reaction", true).await?;
    wait_delivery(&resources, "left", 5).await?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            let input = resources.channel("left", "incoming")?.progress().await?;
            if input.accepted == 6 && input.processed["query"] == 6 {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    core.shutdown().await?;
    drop((core, resources));
    let (core, resources) = open(directory.path(), true).await?;
    core.start().await?;
    resources.append("left", 6).await?;
    let graph = core.get_computation_graph("left").await?;
    wait_delivery(&resources, "left", 6)
        .await
        .with_context(|| format!("transport identity after replay: {:?}", graph.observed()))?;
    core.shutdown().await?;
    assert_eq!(
        effect_sequences(directory.path(), "left")?,
        [1, 2, 3, 4, 5, 6]
    );
    Ok(())
}

#[tokio::test]
#[ignore = "factory lifecycle crash worker invoked by the parent test"]
async fn factory_lifecycle_crash_worker() -> Result<()> {
    let root = PathBuf::from(std::env::var("DRASI_FACTORY_LIFECYCLE_ROOT")?);
    let (core, resources) = open(&root, true).await?;
    core.apply_desired_state(0, "initial", desired(&root, true)?)
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    core.start().await?;
    for graph in ["left", "right"] {
        resources.append(graph, 1).await?;
        wait_delivery(&resources, graph, 1).await?;
    }
    stage(&core, "left", "query", false).await?;
    resources.append("left", 2).await?;
    tokio::time::timeout(DEADLINE, async {
        loop {
            let progress = resources.channel("left", "incoming")?.progress().await?;
            if progress.accepted == 2 && progress.processed["query"] == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    std::process::exit(89);
}

#[tokio::test]
async fn process_restart_restores_bulk_factories_and_pending_input_without_resubmitting_configuration(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let worker = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "factory_lifecycle_crash_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("DRASI_FACTORY_LIFECYCLE_ROOT", directory.path())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    assert_eq!(
        worker.status.code(),
        Some(89),
        "{}\n{}",
        String::from_utf8_lossy(&worker.stdout),
        String::from_utf8_lossy(&worker.stderr)
    );
    let (core, resources) = open(directory.path(), false).await?;
    assert_eq!(core.desired_configuration()?.revision, 1);
    assert_eq!(core.desired_configuration()?.desired.graphs.len(), 2);
    assert!(
        !core.management_status().await?.converged(),
        "missing factory must remain visible"
    );
    core.register_component_factory(Arc::new(ReactionFactory::new()?))
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    core.start().await?;
    wait_delivery(&resources, "left", 2).await?;
    wait_delivery(&resources, "right", 1).await?;
    resources.append("left", 3).await?;
    resources.append("right", 2).await?;
    wait_delivery(&resources, "left", 3).await?;
    wait_delivery(&resources, "right", 2).await?;
    core.shutdown().await?;
    for (graph, expected) in [("left", vec![1, 2, 3]), ("right", vec![1, 2])] {
        assert_eq!(effect_sequences(directory.path(), graph)?, expected);
    }
    Ok(())
}
