// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationIndexes, ComputationResourceCleanup},
    interface::IndexError,
    models::{ElementMetadata, ElementReference, SourceChange},
};
use drasi_host_sdk::{
    management::{HostManagementResources, SourceTimeMergeStorageConfig},
    PluginRegistry,
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::{
    computation::v1::*,
    management::{DesiredInstance, ManagementResourceResolver},
    DrasiLib, MemorySecretStoreProvider,
};
use serde_json::json;
use tokio::sync::{mpsc, watch, Mutex, Notify};

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("component ID")
}
fn resource(value: &str) -> ResourceId {
    ResourceId::try_new(value).expect("resource ID")
}
fn port(value: &str) -> PortId {
    PortId::try_new(value).expect("port ID")
}
fn stream(value: &str) -> StreamId {
    StreamId::try_new(value).expect("stream ID")
}
fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("positive size")
}

#[derive(Default)]
struct Gate {
    enabled: AtomicBool,
    entered: Notify,
    release: Notify,
}

struct Fixtures {
    input: Arc<Mutex<mpsc::Receiver<ChangeEnvelope>>>,
    output: watch::Sender<Vec<(String, ChangeEnvelope)>>,
    fail: AtomicBool,
    creates: AtomicUsize,
    gate: Gate,
}

fn fixtures() -> (Arc<Fixtures>, mpsc::Sender<ChangeEnvelope>) {
    let (sender, receiver) = mpsc::channel(8);
    (
        Arc::new(Fixtures {
            input: Arc::new(Mutex::new(receiver)),
            output: watch::channel(Vec::new()).0,
            fail: AtomicBool::new(false),
            creates: AtomicUsize::new(0),
            gate: Gate::default(),
        }),
        sender,
    )
}

struct Factory {
    descriptor: FactoryDescriptor,
    fixtures: Arc<Fixtures>,
}

impl Factory {
    fn new(source: bool, fixtures: Arc<Fixtures>) -> Arc<Self> {
        Arc::new(Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new(
                    if source {
                        "test/recipe-source"
                    } else {
                        "test/recipe-reaction"
                    },
                    "1",
                )
                .expect("implementation"),
                role: if source {
                    ComponentRole::Source
                } else {
                    ComponentRole::Sink
                },
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([(
                        Arc::from("credential"),
                        ConfigurationField {
                            value_type: ConfigurationType::String,
                            required: source,
                            secret: true,
                        },
                    )]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::new(),
            },
            fixtures,
        })
    }

    fn specification(&self, name: &str) -> Result<ComponentSpecification> {
        let source = self.descriptor.role == ComponentRole::Source;
        Ok(ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(
                id(name),
                vec![PortDescriptor::new(
                    port(if source { "out" } else { "in" }),
                    if source {
                        PortDirection::Output
                    } else {
                        PortDirection::Input
                    },
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?
            .with_semantic_kind(if source {
                ComponentSemanticKind::Source
            } else {
                ComponentSemanticKind::Reaction
            }),
            role: self.descriptor.role,
            completion: (!source).then_some(SinkCompletion::Handled),
            implementation: self.descriptor.implementation.clone(),
            configuration_version: self.descriptor.configuration_version,
            configuration: if source {
                BTreeMap::from([(
                    Arc::from("credential"),
                    ConfigurationValue::Reference {
                        resource: resource("credentials"),
                        key: "secret:token".into(),
                        secret: true,
                    },
                )])
            } else {
                BTreeMap::new()
            },
            dependencies: BTreeMap::new(),
        })
    }
}

struct Component {
    descriptor: ComponentDescriptor,
    configuration: serde_json::Value,
    fixtures: Arc<Fixtures>,
}

#[async_trait]
impl ComputationComponent for Component {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(self.configuration.clone())
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for Component {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self
            .fixtures
            .input
            .lock()
            .await
            .recv()
            .await
            .map(|envelope| OutputEnvelope {
                port: port("out"),
                envelope,
            }))
    }
}

#[async_trait]
impl EnvelopeSink for Component {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.fixtures.output.send_modify(|output| {
            output.push((self.descriptor.id().to_string(), input.envelope));
        });
        Ok(())
    }
}

#[async_trait]
impl ComponentFactory for Factory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, _: &ComponentSpecification) -> Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        if self.descriptor.role == ComponentRole::Source {
            self.fixtures.creates.fetch_add(1, Ordering::SeqCst);
            if self.fixtures.gate.enabled.load(Ordering::Acquire) {
                self.fixtures.gate.entered.notify_one();
                self.fixtures.gate.release.notified().await;
            }
            if self.fixtures.fail.load(Ordering::Acquire) {
                return Err(ComponentCreationError::retryable(anyhow::anyhow!(
                    "injected source construction failure"
                )));
            }
        }
        let component = Box::new(Component {
            descriptor: context.specification.descriptor.clone(),
            configuration: serde_json::to_value(context.configuration())
                .map_err(ComponentCreationError::terminal)?,
            fixtures: self.fixtures.clone(),
        });
        Ok(if self.descriptor.role == ComponentRole::Source {
            ConstructedComponent::source(component)
        } else {
            ConstructedComponent::sink(component)
        })
    }
}

struct CountedProvider {
    inner: Arc<dyn ComputationIndexProvider>,
    open: Arc<AtomicUsize>,
}

struct CountedCleanup {
    inner: Arc<dyn ComputationResourceCleanup>,
    open: Arc<AtomicUsize>,
    closed: AtomicBool,
}

#[async_trait]
impl ComputationResourceCleanup for CountedCleanup {
    fn cancel(&self) {
        self.inner.cancel();
    }
    async fn shutdown(&self) -> std::result::Result<(), IndexError> {
        self.inner.shutdown().await?;
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.open.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(())
    }
    async fn quiesce(&self) -> std::result::Result<(), IndexError> {
        self.inner.quiesce().await
    }
}

#[async_trait]
impl ComputationIndexProvider for CountedProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &str,
    ) -> std::result::Result<ComputationIndexes, IndexError> {
        let indexes = self.inner.create_indexes(graph, component).await?;
        let cleanup = indexes.cleanup().expect("persistent cleanup").clone();
        self.open.fetch_add(1, Ordering::SeqCst);
        Ok(indexes.with_cleanup(Arc::new(CountedCleanup {
            inner: cleanup,
            open: self.open.clone(),
            closed: AtomicBool::new(false),
        })))
    }
    fn is_volatile(&self) -> bool {
        self.inner.is_volatile()
    }
    fn durability(&self) -> drasi_core::interface::StorageDurability {
        self.inner.durability()
    }
}

fn provider(path: &std::path::Path, open: Arc<AtomicUsize>) -> Arc<dyn ComputationIndexProvider> {
    Arc::new(CountedProvider {
        inner: Arc::new(RocksDbComputationProvider::new(
            path,
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("memory budget"),
            ),
        )),
        open,
    })
}

fn storage_identity() -> ImplementationIdentity {
    ImplementationIdentity::try_new("test/rocksdb-provider", "1").expect("storage identity")
}

#[tokio::test]
async fn unversioned_provider_bindings_remain_supported_but_do_not_invent_an_identity() -> Result<()>
{
    let provider: Arc<dyn ComputationIndexProvider> =
        Arc::new(drasi_core::computation::InMemoryComputationProvider);
    let resolver = HostManagementResources::new(
        &PluginRegistry::new(),
        Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
        None,
    )?
    .with_index_provider("external", provider.clone())?;
    let specification = ResourceSpecification {
        id: resource("indexes"),
        role: ResourceRole::IndexBackend,
        ownership: ResourceOwnership::Borrowed,
        binding: "external".into(),
    };
    let mut recipe = json!({"kind":"indexes","provider":"external"});
    let handle = resolver
        .resolve("instance", "graph", &specification, &recipe)
        .await?;
    assert!(Arc::ptr_eq(
        &provider,
        &handle.get::<QueryIndexProviderResource>()?.0
    ));
    recipe["implementation"] = serde_json::to_value(storage_identity())?;
    recipe["configurationVersion"] = 1.into();
    assert!(resolver
        .resolve("instance", "graph", &specification, &recipe)
        .await
        .is_err());
    Ok(())
}

fn add_resource(
    desired: &mut DesiredInstance,
    name: &str,
    role: ResourceRole,
    recipe: serde_json::Value,
) {
    desired.topology.resources.push(ResourceSpecification {
        id: resource(name),
        role,
        ownership: ResourceOwnership::Graph,
        binding: name.into(),
    });
    desired
        .topology
        .resource_configurations
        .insert(resource(name), recipe);
}

fn add_component(
    desired: &mut DesiredInstance,
    specification: ComponentSpecification,
    streams: &[(&str, &str)],
) {
    desired.topology.components.push(DesiredComponent {
        descriptor: specification.descriptor.clone(),
        role: specification.role,
        completion: specification.completion,
        streams: streams
            .iter()
            .map(|(name, value)| (port(name), stream(value)))
            .collect(),
        lifecycle: LifecyclePolicy::default(),
        input_merge: InputMergePolicy::default(),
        construction: ComponentConstruction::Factory(specification),
    });
}

fn definition(source: &Factory, sink: &Factory, persistent: bool) -> Result<DesiredInstance> {
    let mut desired = DesiredInstance::default();
    add_resource(
        &mut desired,
        "credentials",
        ResourceRole::SecretStore,
        json!({"kind":"configuration"}),
    );
    add_component(
        &mut desired,
        source.specification("source")?,
        &[("out", "source/out")],
    );
    add_component(&mut desired, sink.specification("main")?, &[]);
    add_component(&mut desired, sink.specification("late")?, &[]);
    let merger = SourceTimeMergeDefinition {
        graph_id: desired.topology.graph_id.clone(),
        id: id("merge"),
        sources: vec![stream("source/out")],
        output_stream: stream("merge/out"),
        late_output_stream: Some(stream("merge/late")),
        reorder_window_ms: 0,
        max_wait_ms: 5,
        idle_timeout_ms: None,
        max_buffered_events: size(8),
        max_buffered_bytes: size(64 * 1024),
        late_policy: LateEventPolicy::Route,
    };
    add_component(
        &mut desired,
        merger.specification(persistent.then(|| resource("merge-storage")))?,
        &[("out", "merge/out"), ("late", "merge/late")],
    );
    if persistent {
        add_resource(
            &mut desired,
            "indexes",
            ResourceRole::IndexBackend,
            json!({
                "kind":"indexes", "provider":"disk", "implementation":storage_identity(),
                "configurationVersion":1
            }),
        );
        let mut recipe = serde_json::to_value(SourceTimeMergeStorageConfig::new(
            id("merge"),
            resource("indexes"),
        )?)?;
        recipe["kind"] = "sourceTimeMergeStorage".into();
        add_resource(
            &mut desired,
            "merge-storage",
            ResourceRole::IndexBackend,
            recipe,
        );
        desired.topology.resource_dependencies.insert(
            resource("merge-storage"),
            BTreeMap::from([(resource("indexes"), ResourceRole::IndexBackend)]),
        );
    }
    for (from, output, to) in [
        ("source", "out", "merge"),
        ("merge", "out", "main"),
        ("merge", "late", "late"),
    ] {
        let pipe = if persistent && from == "merge" {
            let channel = QosChannelDefinition {
                stream: stream(&format!("merge/{output}")),
                capacity: size(8),
                durable: true,
                retention: RetentionPolicy::Backpressure,
                subscribers: BTreeMap::from([(to.into(), SubscriptionStart::Earliest)]),
            };
            add_resource(
                &mut desired,
                &format!("{to}-journal"),
                ResourceRole::StateStore,
                json!({
                    "kind":"qos", "provider":"disk", "definition":channel,
                    "recovery":{"kind":"replay","failureScope":"processRestart","receiptCapacity":8}
                }),
            );
            DesiredPipe::Qos(QosPipeConfig {
                resource: resource(&format!("{to}-journal")),
                subscriber: to.into(),
                definition: channel,
                gap_policy: ReplayGapPolicy::Strict,
                shared_storage: None,
            })
        } else {
            DesiredPipe::Bounded { capacity: 8 }
        };
        desired.topology.relationships.push(DesiredRelationship {
            definition: EdgeDefinition::new(
                Endpoint::new(id(from), port(output)),
                Endpoint::new(id(to), port("in")),
            ),
            policy: RelationshipPolicy::default(),
            pipe,
        });
    }
    desired.normalized()
}

fn factories(source: Option<Arc<Factory>>, sink: Arc<Factory>) -> Result<FactoryRegistry> {
    let mut registry = FactoryRegistry::standard();
    if let Some(source) = source {
        registry.register(source)?;
    }
    registry.register(sink)?;
    Ok(registry)
}

async fn instance(
    factories: FactoryRegistry,
    secrets: Arc<MemorySecretStoreProvider>,
    provider: Arc<dyn ComputationIndexProvider>,
) -> Result<DrasiLib> {
    instance_for("recipe-instance", factories, secrets, provider).await
}

async fn instance_for(
    name: &str,
    factories: FactoryRegistry,
    secrets: Arc<MemorySecretStoreProvider>,
    provider: Arc<dyn ComputationIndexProvider>,
) -> Result<DrasiLib> {
    let resources = HostManagementResources::new(
        &PluginRegistry::new(),
        Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
        Some(secrets),
    )?
    .with_versioned_index_provider("disk", storage_identity(), 1, provider)?;
    Ok(DrasiLib::builder()
        .with_id(name)
        .with_component_factories(factories)
        .with_management_resources(Arc::new(resources))
        .build()
        .await?)
}

fn event(sequence: u64, timestamp: i64) -> Result<ChangeEnvelope> {
    Ok(GraphChangeCodec::encode_change(
        SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", &sequence.to_string()),
                labels: Arc::from([]),
                effective_from: u64::try_from(timestamp)?,
            },
        },
        stream("source/out"),
        sequence,
        Some(chrono::DateTime::from_timestamp_millis(timestamp).context("timestamp")?),
    )?)
}

async fn output(fixtures: &Fixtures, count: usize) -> Result<Vec<(String, ChangeEnvelope)>> {
    let mut receiver = fixtures.output.subscribe();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = receiver.borrow_and_update().clone();
            if current.len() >= count {
                return Ok(current);
            }
            receiver.changed().await?;
        }
    })
    .await?
}

async fn prepare(
    core: &DrasiLib,
    definition: DesiredTopology,
    factories: FactoryRegistry,
    secrets: Arc<MemorySecretStoreProvider>,
) -> Result<ComponentBatch> {
    let resolver = Arc::new(HostManagementResources::new(
        &PluginRegistry::new(),
        Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
        Some(secrets),
    )?);
    let mut bindings = TopologyBindings {
        factories,
        ..Default::default()
    };
    for specification in &definition.resources {
        bindings.resource_constructors.insert(
            specification.id.clone(),
            drasi_lib::management::resource_constructor(
                resolver.clone(),
                core.computation_plugin_services()?.scope.to_string(),
                definition.graph_id.clone(),
                specification.clone(),
                definition.resource_configurations[&specification.id].clone(),
            ),
        );
    }
    Ok(ComponentBatch {
        definition,
        bindings,
    })
}

#[tokio::test]
async fn ordinary_factory_facades_retain_failures_retry_and_restore_the_same_connections(
) -> Result<()> {
    let (fixtures, sender) = fixtures();
    let source = Factory::new(true, fixtures.clone());
    let reaction = Factory::new(false, fixtures.clone());
    fixtures.fail.store(true, Ordering::Release);
    let secrets = Arc::new(MemorySecretStoreProvider::new().with_secret("token", "resolved-token"));
    let core = DrasiLib::builder()
        .with_id("facade-instance")
        .build()
        .await?;
    let mut phase = "declaring configuration resources";
    let result = tokio::time::timeout(Duration::from_secs(10), async {
    let mut resources = DesiredInstance::default();
    add_resource(
        &mut resources,
        "credentials",
        ResourceRole::SecretStore,
        json!({"kind":"configuration"}),
    );
    core.add_components(
        prepare(
            &core,
            resources.topology,
            FactoryRegistry::default(),
            secrets.clone(),
        )
        .await?,
    )
    .await?;
    phase = "connecting the declared source and reaction";
    let declaration = source.specification("source")?;
    phase = "declaring a source without its factory";
    let waiting = core
        .add_source_from_factory(
            ComponentAddition::declaration(declaration.clone())
                .bind_stream(port("out"), stream("source/out")),
        )
        .await?;
    let mut observations = core.computation_control()?.subscribe_observed();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting = observations.borrow_and_update().components.get(&id("source")).cloned();
            if waiting.is_some_and(|state| state.waiting_for.iter().any(|wait| {
                matches!(wait, DependencyWait::Factory { implementation } if implementation == &declaration.implementation)
            })) {
                return Ok::<_, anyhow::Error>(());
            }
            observations.changed().await?;
        }
    }).await??;
    phase = "declaring the reaction";
    let sink = core
        .add_reaction_from_factory(ComponentAddition::from_specification(
            reaction.specification("reaction")?,
            reaction.clone(),
        ))
        .await?;
    core.computation_control()?
        .connect(
            EdgeDefinition::new(
                Endpoint::new(id("source"), port("out")),
                Endpoint::new(id("reaction"), port("in")),
            ),
            Box::new(BoundedPipeConfig { capacity: 8 }),
            RelationshipPolicy::default(),
        )
        .await?;
        phase = "registering the missing factory";
        core.register_component_factory(source.clone()).await?;
        phase = "observing failed source construction";
    assert!(waiting.wait_created().await.is_err());
    assert_eq!(
        waiting.observed()?.realization,
        RealizationState::CreationFailed
    );
    assert!(core.list_sources().await?.iter().any(|(name, status)| {
        name == "source" && *status == drasi_lib::ComponentStatus::Error
    }));
    assert_eq!(sink.observed()?.realization, RealizationState::Blocked);
    assert!(core
        .list_reactions()
        .await?
        .iter()
        .any(|(name, _)| name == "reaction"));

    fixtures.fail.store(false, Ordering::Release);
    phase = "retrying source construction through start_source";
    core.start_source("source").await?;
    phase = "observing reaction construction after its source is available";
    sink.wait_created().await?;
    phase = "starting the reaction";
    core.start_reaction("reaction").await?;
    phase = "delivering an event";
    sender.send(event(1, 100)?).await?;
    assert_eq!(output(&fixtures, 1).await?[0].0, "reaction");
    let count = fixtures.creates.load(Ordering::SeqCst);
    phase = "exporting reconstructible configuration";
    let exported = core
        .snapshot_computation_configuration()
        .await?
        .native_components
        .context("native recipes")?
        .reconstructible_topology()?;
    assert_eq!(
        fixtures.creates.load(Ordering::SeqCst),
        count,
        "export never constructs an object"
    );
    assert_eq!(exported.components.len(), 2);
    assert_eq!(exported.relationships.len(), 1);
    assert!(!serde_json::to_string(&exported)?.contains("resolved-token"));
    phase = "shutting down the first instance";
    core.shutdown().await?;

    phase = "constructing a clean instance";
    let core = DrasiLib::builder()
        .with_id("facade-instance")
        .build()
        .await?;
    let report = core
        .add_components(
            prepare(&core, exported, factories(Some(source), reaction)?, secrets).await?,
        )
        .await?;
    assert_eq!(report.summary, OperationSummary::Completed);
    phase = "starting the restored instance";
    core.start().await?;
    phase = "delivering a restored event";
    sender.send(event(2, 200)?).await?;
    assert_eq!(output(&fixtures, 2).await?[1].0, "reaction");
    phase = "shutting down the restored instance";
    core.shutdown().await?;
    Ok(())
    }).await;
    result.with_context(|| {
        format!(
            "timed out while {phase}; graph observation: {:?}",
            core.computation_control().map(|control| control.observed())
        )
    })?
}

#[tokio::test]
async fn merger_recipes_restore_connections_scope_and_not_processing_history() -> Result<()> {
    for persistent in [false, true] {
        let directory = tempfile::tempdir()?;
        let open = Arc::new(AtomicUsize::new(0));
        let storage = provider(directory.path(), open.clone());
        let (fixtures, sender) = fixtures();
        let source = Factory::new(true, fixtures.clone());
        let sink = Factory::new(false, fixtures.clone());
        let registry = factories(Some(source.clone()), sink.clone())?;
        let secrets =
            Arc::new(MemorySecretStoreProvider::new().with_secret("token", "resolved-token"));
        let desired = definition(&source, &sink, persistent)?;
        let core = instance(registry.clone(), secrets.clone(), storage.clone()).await?;
        core.apply_desired_state(0, "install", desired).await?;
        assert!(core.reconcile_desired_state().await?.converged());
        core.start().await?;
        sender.send(event(1, 100)?).await?;
        let first = output(&fixtures, 1).await?;
        assert_eq!(first[0].0, "main");
        let progress = GraphProducerProgress::from_envelope(&first[0].1)?.context("producer")?;
        assert_eq!(progress.identity().construction_scope(), "recipe-instance");
        let snapshot = core.snapshot_computation_configuration().await?;
        let exported = snapshot
            .native_components
            .context("native recipes")?
            .reconstructible_topology()?;
        let saved = core.desired_configuration()?.desired;
        assert_eq!(exported.relationships, saved.topology.relationships);
        assert_eq!(
            exported.resource_configurations,
            saved.topology.resource_configurations
        );
        assert!(!serde_json::to_string(&exported)?.contains("resolved-token"));
        assert!(serde_json::to_string(&exported)?.contains("secret:token"));
        core.shutdown().await?;
        drop(core);
        assert_eq!(
            open.load(Ordering::SeqCst),
            0,
            "all acquired bundles were explicitly cleaned"
        );

        let core = instance(registry.clone(), secrets.clone(), storage.clone()).await?;
        core.apply_desired_state(0, "restore", saved.clone())
            .await?;
        assert!(core.reconcile_desired_state().await?.converged());
        assert_eq!(core.desired_configuration()?.desired, saved);
        core.start().await?;
        sender.send(event(2, 50)?).await?;
        let restored = output(&fixtures, 2).await?;
        assert_eq!(restored[1].0, if persistent { "late" } else { "main" });
        let restored_progress =
            GraphProducerProgress::from_envelope(&restored[1].1)?.context("producer")?;
        assert_eq!(
            restored_progress.identity().construction_scope(),
            "recipe-instance"
        );
        core.shutdown().await?;
        assert_eq!(open.load(Ordering::SeqCst), 0);
        if persistent {
            let core = instance_for("different-instance", registry, secrets, storage).await?;
            core.apply_desired_state(0, "isolated", saved).await?;
            assert!(core.reconcile_desired_state().await?.converged());
            core.start().await?;
            sender.send(event(1, 25)?).await?;
            let isolated = output(&fixtures, 3).await?;
            assert_eq!(
                isolated[2].0, "main",
                "a different instance must not adopt the old frontier"
            );
            let progress =
                GraphProducerProgress::from_envelope(&isolated[2].1)?.context("producer")?;
            assert_eq!(
                progress.identity().construction_scope(),
                "different-instance"
            );
            core.shutdown().await?;
            assert_eq!(open.load(Ordering::SeqCst), 0);
        }
    }
    Ok(())
}

#[tokio::test]
async fn incompatible_provider_recipe_versions_remain_visible_without_opening_merger_storage(
) -> Result<()> {
    for (name, field, value) in [
        ("indexes", "configurationVersion", json!(2)),
        (
            "indexes",
            "implementation",
            json!(ImplementationIdentity::try_new(
                "test/rocksdb-provider",
                "2"
            )?),
        ),
        ("merge-storage", "configurationVersion", json!(2)),
        (
            "merge-storage",
            "implementation",
            json!(ImplementationIdentity::try_new(
                "drasi/source-time-merge-storage",
                "2"
            )?),
        ),
    ] {
        let directory = tempfile::tempdir()?;
        let open = Arc::new(AtomicUsize::new(0));
        let storage = provider(directory.path(), open.clone());
        let (fixtures, _sender) = fixtures();
        let source = Factory::new(true, fixtures.clone());
        let sink = Factory::new(false, fixtures);
        let mut desired = definition(&source, &sink, true)?;
        desired
            .topology
            .resource_configurations
            .get_mut(&resource(name))
            .context("recipe")?[field] = value;
        let secrets =
            Arc::new(MemorySecretStoreProvider::new().with_secret("token", "resolved-token"));
        let core = instance(factories(Some(source), sink)?, secrets, storage).await?;
        core.apply_desired_state(0, "restore", desired.clone())
            .await?;
        let status = core.reconcile_desired_state().await?;
        assert!(!status.converged());
        assert!(
            status.resource_errors.contains_key(&resource(name)),
            "{status:?}"
        );
        assert_eq!(core.desired_configuration()?.desired, desired);
        assert!(!core
            .computation_component("merge")?
            .observed()?
            .waiting_for
            .is_empty());
        core.shutdown().await?;
        assert_eq!(open.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

struct EmptyCleanup;

#[async_trait]
impl ComputationResourceCleanup for EmptyCleanup {
    fn cancel(&self) {}
    async fn shutdown(&self) -> std::result::Result<(), IndexError> {
        Ok(())
    }
}

struct InvalidPersistentProvider(Arc<AtomicUsize>);

#[async_trait]
impl ComputationIndexProvider for InvalidPersistentProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &str,
    ) -> std::result::Result<ComputationIndexes, IndexError> {
        let indexes = drasi_core::computation::InMemoryComputationProvider
            .create_indexes(graph, component)
            .await?;
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(indexes.with_cleanup(Arc::new(CountedCleanup {
            inner: Arc::new(EmptyCleanup),
            open: self.0.clone(),
            closed: AtomicBool::new(false),
        })))
    }
    fn is_volatile(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn rejected_storage_bundle_keeps_its_cleanup_owner_until_graph_shutdown() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let open = Arc::new(AtomicUsize::new(0));
    let invalid_open = Arc::new(AtomicUsize::new(0));
    let (fixtures, _sender) = fixtures();
    let source = Factory::new(true, fixtures.clone());
    let sink = Factory::new(false, fixtures);
    let mut desired = definition(&source, &sink, true)?;
    desired
        .topology
        .resource_configurations
        .get_mut(&resource("indexes"))
        .context("indexes")?["provider"] = "invalid".into();
    let resources = HostManagementResources::new(
        &PluginRegistry::new(),
        Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
        Some(Arc::new(
            MemorySecretStoreProvider::new().with_secret("token", "resolved-token"),
        )),
    )?
    .with_index_provider("disk", provider(directory.path(), open.clone()))?
    .with_versioned_index_provider(
        "invalid",
        storage_identity(),
        1,
        Arc::new(InvalidPersistentProvider(invalid_open.clone())),
    )?;
    let core = DrasiLib::builder()
        .with_id("recipe-instance")
        .with_component_factories(factories(Some(source), sink)?)
        .with_management_resources(Arc::new(resources))
        .build()
        .await?;
    core.apply_desired_state(0, "invalid-provider", desired)
        .await?;
    assert!(!core.reconcile_desired_state().await?.converged());
    assert_eq!(
        core.computation_component("merge")?.observed()?.realization,
        RealizationState::CreationFailed
    );
    assert_eq!(
        invalid_open.load(Ordering::SeqCst),
        1,
        "failed bundle is retained, not repeatedly opened"
    );
    core.shutdown().await?;
    assert_eq!(
        invalid_open.load(Ordering::SeqCst),
        0,
        "failed acquired bundle was explicitly cleaned"
    );
    assert_eq!(open.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn unavailable_factory_version_secret_and_constructor_retain_retryable_recipes() -> Result<()>
{
    for failure in [
        "factory",
        "implementation",
        "configuration",
        "secret",
        "constructor",
    ] {
        let directory = tempfile::tempdir()?;
        let open = Arc::new(AtomicUsize::new(0));
        let storage = provider(directory.path(), open.clone());
        let (fixtures, _sender) = fixtures();
        let source = Factory::new(true, fixtures.clone());
        let sink = Factory::new(false, fixtures.clone());
        let saved = definition(&source, &sink, true)?;
        let mut available = Factory::new(true, fixtures.clone());
        if failure == "implementation" {
            Arc::get_mut(&mut available)
                .context("unshared factory")?
                .descriptor
                .implementation
                .version = "2".into();
        } else if failure == "configuration" {
            Arc::get_mut(&mut available)
                .context("unshared factory")?
                .descriptor
                .configuration_version = 2;
        }
        let secrets = Arc::new(MemorySecretStoreProvider::new());
        if failure != "secret" {
            secrets.set_secret("token", "resolved-token").await;
        }
        fixtures
            .fail
            .store(failure == "constructor", Ordering::Release);
        let core = instance(
            factories((failure != "factory").then_some(available), sink.clone())?,
            secrets.clone(),
            storage.clone(),
        )
        .await?;
        core.apply_desired_state(0, "restore", saved.clone())
            .await?;
        assert!(
            !core.reconcile_desired_state().await?.converged(),
            "{failure}"
        );
        assert_eq!(core.desired_configuration()?.desired, saved);
        let observed = core.computation_component("source")?.observed()?;
        assert!(
            observed.realization == RealizationState::CreationFailed
                || !observed.waiting_for.is_empty(),
            "{failure}: {observed:?}"
        );
        core.shutdown().await?;
        drop(core);
        assert_eq!(
            open.load(Ordering::SeqCst),
            0,
            "{failure}: resources must not be orphaned"
        );

        fixtures.fail.store(false, Ordering::Release);
        secrets.set_secret("token", "resolved-token").await;
        let core = instance(factories(Some(source), sink)?, secrets, storage).await?;
        core.apply_desired_state(0, "retry", saved).await?;
        assert!(
            core.reconcile_desired_state().await?.converged(),
            "{failure}"
        );
        core.shutdown().await?;
        assert_eq!(open.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test]
async fn cancelled_restoration_wait_keeps_declarations_and_cleanup_owned() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let open = Arc::new(AtomicUsize::new(0));
    let storage = provider(directory.path(), open.clone());
    let (fixtures, _sender) = fixtures();
    fixtures.gate.enabled.store(true, Ordering::Release);
    let source = Factory::new(true, fixtures.clone());
    let sink = Factory::new(false, fixtures.clone());
    let saved = definition(&source, &sink, true)?;
    let registry = factories(Some(source), sink)?;
    let secrets = Arc::new(MemorySecretStoreProvider::new().with_secret("token", "resolved-token"));
    let core = instance(registry.clone(), secrets.clone(), storage.clone()).await?;
    core.apply_desired_state(0, "restore", saved.clone())
        .await?;
    tokio::time::timeout(Duration::from_secs(5), fixtures.gate.entered.notified()).await?;
    let waiter = tokio::spawn({
        let core = core.clone();
        async move { core.reconcile_desired_state().await }
    });
    waiter.abort();
    assert!(waiter.await.expect_err("cancelled waiter").is_cancelled());
    assert_eq!(core.desired_configuration()?.desired, saved);
    assert_eq!(
        core.computation_component("source")?
            .observed()?
            .realization,
        RealizationState::Creating
    );
    tokio::time::timeout(Duration::from_secs(5), core.shutdown()).await??;
    drop(core);
    assert_eq!(open.load(Ordering::SeqCst), 0);

    fixtures.gate.enabled.store(false, Ordering::Release);
    let core = instance(registry, secrets, storage).await?;
    core.apply_desired_state(0, "retry", saved).await?;
    assert!(core.reconcile_desired_state().await?.converged());
    core.shutdown().await?;
    assert_eq!(open.load(Ordering::SeqCst), 0);
    Ok(())
}
