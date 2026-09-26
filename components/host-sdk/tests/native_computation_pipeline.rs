// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Build the producer separately before running:
//! CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo build --offline -p drasi-computation-standard --features dynamic-plugin
//! CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo test --offline -p drasi-host-sdk --test native_computation_pipeline
//!
//! There is deliberately no Rust dependency on drasi-computation-standard here.
//! All component calls use computation::load and the exported C tables.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider,
    interface::{CreatedIndexes, ElementIndex, ElementStream, IndexBackendPlugin, IndexError},
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, QueryJoin,
        SourceChange,
    },
    path_solver::match_path::MatchPath,
};
use drasi_host_sdk::computation::{self, NativeFactory, NativePlugin};
use drasi_host_sdk::{
    callbacks,
    loader::{load_plugin_family_from_path, scan_plugin_metadata, LoadedPluginFamily},
    plugin_registry::computation_plugin_id,
    PluginCategory, PluginLoader, PluginLoaderConfig, PluginRegistry,
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::computation::v1::*;
use serde_json::{json, Value};
use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};
use tokio::sync::Notify;

const COUNTER: &str = "drasi.standard/volatile-counter";
const MIDDLEWARE: &str = "drasi.standard/middleware";
const CAPTURE: &str = "drasi.standard/capture";
const ARITHMETIC: &str = "drasi.standard/arithmetic";
const TIMEOUT: Duration = Duration::from_secs(30);

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("components")
        .parent()
        .expect("workspace")
        .to_owned()
}
fn plugin_path() -> PathBuf {
    if let Some(path) = std::env::var_os("DRASI_NATIVE_STANDARD_PLUGIN") {
        return path.into();
    }
    let target = match std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        Some(path) => workspace().join(path),
        None => workspace().join("target"),
    };
    target.join("debug").join(format!(
        "{}drasi_computation_standard{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ))
}
fn plugin() -> Result<Arc<NativePlugin>> {
    static PLUGIN: OnceLock<std::result::Result<Arc<NativePlugin>, String>> = OnceLock::new();
    PLUGIN.get_or_init(|| {
        let path = plugin_path();
        if !path.is_file() {
            return Err(format!("missing native plugin at {}; first run CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo build --offline -p drasi-computation-standard --features dynamic-plugin (or set DRASI_NATIVE_STANDARD_PLUGIN)", path.display()));
        }
        computation::load(path).map_err(|error| format!("{error:#}"))
    }).clone().map_err(anyhow::Error::msg)
}
fn factory(plugin: &NativePlugin, name: &str) -> Result<Arc<NativeFactory>> {
    plugin
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == name)
        .cloned()
        .with_context(|| format!("missing native factory {name}"))
}
fn directory() -> Result<tempfile::TempDir> {
    let root = workspace().join("target/native-computation-tests");
    std::fs::create_dir_all(&root)?;
    Ok(tempfile::Builder::new()
        .prefix("pipeline-")
        .tempdir_in(root)?)
}
fn id(name: &str) -> ComponentId {
    ComponentId::try_new(name).expect("test component ID")
}
fn stream(name: &str) -> StreamId {
    StreamId::try_new(name).expect("test stream ID")
}
fn endpoint(component: &str, port: &str) -> Endpoint {
    Endpoint::new(id(component), PortId::try_new(port).expect("test port"))
}
fn edge(from: &str, to: &str) -> EdgeDefinition {
    EdgeDefinition::new(endpoint(from, "out"), endpoint(to, "in"))
}
fn codec() -> Result<EnvelopeCodec> {
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(64 * 1024 * 1024).expect("limit"));
    codec.register_schema(GraphChangeCodec::schema())?;
    Ok(codec)
}

#[tokio::test(flavor = "current_thread")]
async fn managed_redb_instance_reconstructs_real_plugin_factories_and_preserves_definitions(
) -> Result<()> {
    use drasi_lib::{management::*, DrasiLib};
    use drasi_state_store_redb::RedbConfigurationStore;
    let plugin = plugin()?;
    let directory = directory()?;
    let output_path = directory.path().join("managed.jsonl");
    let source = factory(&plugin, COUNTER)?;
    let sink = factory(&plugin, CAPTURE)?;
    let graph = ComputationGraph::builder("managed-native")
        .component(
            source.specification(
                id("counter"),
                json!({
                    "stream":"counter/out","count":3,"start":10,"step":2,"interval_ms":2
                }),
            )?,
            source,
        )
        .component(
            sink.specification(id("capture"), json!({"path":output_path}))?,
            sink,
        )
        .bind_stream(endpoint("counter", "out"), stream("counter/out"))
        .connect(
            edge("counter", "capture"),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .build()?;
    let mut definition = DesiredInstance::from(graph.snapshot().select(GraphSelection::All)?);
    let secret_id = ResourceId::try_new("configuration")?;
    definition.graphs[0]
        .topology
        .resources
        .push(ResourceSpecification {
            id: secret_id.clone(),
            role: ResourceRole::SecretStore,
            ownership: ResourceOwnership::Borrowed,
            binding: Arc::from("host.configuration"),
        });
    definition.graphs[0]
        .topology
        .resource_configurations
        .insert(
            secret_id.clone(),
            json!({"kind":"configuration","secrets":{"capture-path":output_path}}),
        );
    let capture = definition.graphs[0]
        .topology
        .components
        .iter_mut()
        .find(|node| node.descriptor.id() == &id("capture"))
        .unwrap();
    let ComponentConstruction::Factory(spec) = &mut capture.construction else {
        unreachable!()
    };
    spec.configuration.insert(
        Arc::from("path"),
        ConfigurationValue::Reference {
            resource: secret_id,
            key: Arc::from("secret:capture-path"),
            secret: true,
        },
    );
    let store = Arc::new(RedbConfigurationStore::new(
        directory.path().join("management.redb"),
        [19; 32],
    )?);
    let mut plugins = PluginRegistry::new();
    plugins.register_computation_plugin(plugin.clone())?;
    let resources = Arc::new(drasi_host_sdk::management::HostManagementResources::new(
        &plugins,
        Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
        None,
    )?);
    let core = DrasiLib::builder()
        .with_id("managed-host")
        .with_component_factories(plugins.computation_factory_registry()?)
        .with_configuration_store(store.clone())
        .with_management_resources(resources.clone())
        .build()
        .await?;
    let receipt = core
        .apply_desired_state(0, "create", definition.clone())
        .await?;
    assert!(receipt.durable);
    assert!(core.reconcile_desired_state().await?.converged());
    let handle = core.get_computation_graph("managed-native").await?;
    let generation = handle.observed().components[&id("counter")].generation;
    core.apply_desired_state(receipt.revision, "same-config", definition)
        .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    assert_eq!(
        handle.observed().components[&id("counter")].generation,
        generation
    );
    core.start().await?;
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if output_path.exists() && captured(&output_path)?.len() == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        anyhow::Ok(())
    })
    .await??;
    assert_eq!(values(&captured(&output_path)?)?, [10, 12, 14]);
    core.snapshot_desired_configuration("running-definition")
        .await?;
    core.shutdown().await?;
    let restored = DrasiLib::builder()
        .with_id("managed-host")
        .with_component_factories(plugins.computation_factory_registry()?)
        .with_configuration_store(store)
        .with_management_resources(resources)
        .build()
        .await?;
    assert_eq!(restored.desired_configuration()?.revision, receipt.revision);
    assert!(restored.management_status().await?.converged());
    assert_eq!(
        restored
            .get_computation_graph("managed-native")
            .await?
            .observed()
            .components
            .len(),
        2
    );
    assert!(restored
        .load_configuration_snapshot("running-definition")
        .await?
        .is_some());
    restored.shutdown().await?;
    Ok(())
}
fn captured(path: &Path) -> Result<Vec<ChangeEnvelope>> {
    let codec = codec()?;
    std::fs::read(path)?
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| Ok(codec.decode(line)?))
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn managed_graph_constructs_the_existing_query_engine_from_persisted_recipes() -> Result<()> {
    use drasi_lib::{management::*, DrasiLib};
    use drasi_state_store_redb::RedbConfigurationStore;
    struct Sink {
        descriptor: ComponentDescriptor,
        values: Arc<Mutex<Vec<Value>>>,
    }
    #[async_trait]
    impl ComputationComponent for Sink {
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
    #[async_trait]
    impl EnvelopeSink for Sink {
        fn completion(&self) -> SinkCompletion {
            SinkCompletion::Handled
        }
        async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
            for operation in input.envelope.changes().operations() {
                let after = match operation {
                    ChangeOperation::Added { after, .. }
                    | ChangeOperation::Updated { after, .. } => after,
                    _ => anyhow::bail!("unexpected query result"),
                };
                let row = QueryChangeCodec::decode_row(after)?;
                self.values
                    .lock()
                    .unwrap()
                    .push(QueryChangeCodec::row_values_to_json(&row.values));
            }
            Ok(())
        }
    }
    struct SinkFactory {
        descriptor: FactoryDescriptor,
        values: Arc<Mutex<Vec<Value>>>,
    }
    #[async_trait]
    impl ComponentFactory for SinkFactory {
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
            Ok(ConstructedComponent::sink(Box::new(Sink {
                descriptor: context.specification.descriptor.clone(),
                values: self.values.clone(),
            })))
        }
    }
    let plugin = plugin()?;
    let source = factory(&plugin, COUNTER)?;
    let query = Arc::new(ContinuousQueryFactory::default());
    let values = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(SinkFactory {
        descriptor: FactoryDescriptor {
            implementation: ImplementationIdentity::try_new("test/query-sink", "1")?,
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: ConfigurationSchema::default(),
            dependencies: Default::default(),
        },
        values: values.clone(),
    });
    let indexes = ResourceId::try_new("indexes")?;
    let query_definition = ContinuousQueryDefinition {
        graph_id: "managed-query".into(),
        id: id("query"),
        query: "MATCH (n:Counter) RETURN n.value AS value".into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: stream("query/out"),
        outbox_capacity: NonZeroUsize::new(16).unwrap(),
    };
    let query_spec = ComponentSpecification {
        descriptor: query_definition.descriptor(),
        role: ComponentRole::Query,
        completion: None,
        implementation: query.descriptor().implementation.clone(),
        configuration_version: 1,
        configuration: [
            (
                Arc::from("query"),
                ConfigurationValue::Literal(json!(query_definition.query)),
            ),
            (
                Arc::from("language"),
                ConfigurationValue::Literal(json!("cypher")),
            ),
            (
                Arc::from("stream"),
                ConfigurationValue::Literal(json!("query/out")),
            ),
        ]
        .into(),
        dependencies: [(Arc::from("indexes"), vec![indexes.clone()])].into(),
    };
    let sink_spec = ComponentSpecification {
        descriptor: ComponentDescriptor::try_new(
            id("sink"),
            vec![PortDescriptor::new(
                PortId::try_new("in")?,
                PortDirection::Input,
                QueryChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?,
        role: ComponentRole::Sink,
        completion: Some(SinkCompletion::Handled),
        implementation: sink.descriptor.implementation.clone(),
        configuration_version: 1,
        configuration: Default::default(),
        dependencies: Default::default(),
    };
    let graph = ComputationGraph::builder("managed-query")
        .declare_resource(ResourceSpecification {
            id: indexes.clone(),
            role: ResourceRole::IndexBackend,
            ownership: ResourceOwnership::Borrowed,
            binding: Arc::from("memory"),
        })?
        .resource_configuration(indexes, json!({"kind":"memoryIndexes"}))?
        .component(
            source.specification(
                id("counter"),
                json!({"stream":"counter/out","count":3,"start":2,"step":2,"interval_ms":2}),
            )?,
            source,
        )
        .component(query_spec, query)
        .component(sink_spec, sink.clone())
        .bind_stream(endpoint("counter", "out"), stream("counter/out"))
        .bind_stream(endpoint("query", "out"), stream("query/out"))
        .connect(
            edge("counter", "query"),
            Box::new(BoundedPipeConfig { capacity: 4 }),
        )
        .connect(
            edge("query", "sink"),
            Box::new(BoundedPipeConfig { capacity: 4 }),
        )
        .build()?;
    let mut plugins = PluginRegistry::new();
    plugins.register_computation_plugin(plugin)?;
    let mut factories = plugins.computation_factory_registry()?;
    factories.register(sink)?;
    let resources = Arc::new(drasi_host_sdk::management::HostManagementResources::new(
        &plugins,
        Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
        None,
    )?);
    let directory = directory()?;
    let store = Arc::new(RedbConfigurationStore::new(
        directory.path().join("query.redb"),
        [17; 32],
    )?);
    let core = DrasiLib::builder()
        .with_component_factories(factories)
        .with_management_resources(resources)
        .with_configuration_store(store)
        .build()
        .await?;
    core.apply_desired_state(
        0,
        "query",
        DesiredInstance::from(graph.snapshot().select(GraphSelection::All)?),
    )
    .await?;
    assert!(core.reconcile_desired_state().await?.converged());
    core.start().await?;
    tokio::time::timeout(TIMEOUT, async {
        while values.lock().unwrap().len() < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    assert_eq!(
        *values.lock().unwrap(),
        [json!({"value":2}), json!({"value":4}), json!({"value":6})]
    );
    core.shutdown().await?;
    Ok(())
}
fn integer(element: &Element, name: &str) -> Result<i64> {
    let properties = match element {
        Element::Node { properties, .. } | Element::Relation { properties, .. } => properties,
    };
    match properties.get(name) {
        Some(ElementValue::Integer(value)) => Ok(*value),
        _ => anyhow::bail!("missing integer property {name}"),
    }
}
fn row(envelope: &ChangeEnvelope) -> Result<Element> {
    let changes = GraphChangeCodec::decode_changes(envelope)?;
    let [SourceChange::Insert { element } | SourceChange::Update { element }] = changes.as_slice()
    else {
        anyhow::bail!("expected exactly one inserted/updated element");
    };
    Ok(element.clone())
}
fn values(envelopes: &[ChangeEnvelope]) -> Result<Vec<i64>> {
    envelopes
        .iter()
        .map(|envelope| integer(&row(envelope)?, "value"))
        .collect()
}
fn configuration(snapshot: &GraphConfigurationSnapshot, name: &str) -> Result<Value> {
    match snapshot.configurations.get(&id(name)) {
        Some(CapturedComponentConfiguration::Available { values }) => Ok(values.clone()),
        value => anyhow::bail!("configuration for {name} is not available: {value:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn shared_family_loader_and_registry_execute_native_factories() -> Result<()> {
    let direct = plugin()?;
    let path = plugin_path();
    let family = load_plugin_family_from_path(
        &path,
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )?;
    let LoadedPluginFamily::Computation {
        file_path,
        plugin: shared,
    } = family
    else {
        anyhow::bail!("shared loader misclassified the native library as legacy");
    };
    assert_eq!(file_path, path);
    assert_eq!(shared.metadata(), direct.metadata());
    let qualified_id = computation_plugin_id(shared.metadata());
    assert_eq!(
        qualified_id,
        format!(
            "computation:drasi-computation-standard@{}",
            shared.metadata().plugin.version
        )
    );
    let summary = scan_plugin_metadata(&path).context("native metadata-only scan")?;
    assert_eq!(summary.plugin_id, qualified_id);
    assert_eq!(summary.abi_family.as_deref(), Some("computation"));
    assert_eq!(summary.abi_version.as_deref(), Some("1.0.0"));
    assert_eq!(summary.version, shared.metadata().plugin.version.as_ref());

    // Restrict discovery to the existing build artifact: copying a process-pinned
    // library into a temporary directory would prevent cleanup on Windows.
    let loader = PluginLoader::new(PluginLoaderConfig {
        plugin_dir: path.parent().context("plugin directory")?.to_owned(),
        file_patterns: vec![path
            .file_name()
            .context("plugin filename")?
            .to_string_lossy()
            .into_owned()],
    });
    let mut batch = loader.load_all_families(
        std::ptr::null_mut(),
        callbacks::default_log_callback_fn(),
        std::ptr::null_mut(),
        callbacks::default_lifecycle_callback_fn(),
    )?;
    assert!(batch.failures.is_empty(), "{:?}", batch.failures);
    assert_eq!(batch.loaded.len(), 1);
    let LoadedPluginFamily::Computation {
        file_path,
        plugin: discovered,
    } = batch.loaded.pop().context("discovered native library")?
    else {
        anyhow::bail!("directory discovery misclassified the native library");
    };
    assert_eq!(file_path, path);
    assert_eq!(discovered.metadata(), shared.metadata());

    let mut registry = PluginRegistry::new();
    let kinds = registry.register_computation_plugin(discovered.clone())?;
    assert_eq!(registry.version(), 1);
    assert!(!registry.is_empty());
    assert_eq!(registry.descriptor_count(), 4);
    assert!(registry.source_kinds().is_empty());
    assert!(registry.reaction_kinds().is_empty());
    assert_eq!(kinds.len(), 4);
    for kind in &kinds {
        assert_eq!(kind.category, PluginCategory::Computation);
        assert_eq!(kind.config_version, "1");
        assert_eq!(kind.config_schema_name, kind.kind);
    }
    let mut names: Vec<_> = kinds.iter().map(|kind| kind.kind.as_str()).collect();
    names.sort_unstable();
    let mut expected = [COUNTER, MIDDLEWARE, CAPTURE, ARITHMETIC];
    expected.sort_unstable();
    assert_eq!(names, expected);
    assert_eq!(
        registry.computation_plugin_metadata(),
        vec![discovered.metadata().clone()]
    );
    assert!(registry.register_computation_plugin(shared).is_err());
    assert_eq!(
        registry.version(),
        1,
        "duplicate admission is not a mutation"
    );
    assert_eq!(registry.descriptor_count(), 4);

    let directory = directory()?;
    let capture_path = directory.path().join("shared-loader.jsonl");
    let source = factory(&discovered, COUNTER)?;
    let sink = factory(&discovered, CAPTURE)?;
    let desired = ComputationGraph::builder("shared-family")
        .component(
            source.specification(
                id("counter"),
                json!({"stream":"counter/out","count":2,"start":7}),
            )?,
            source,
        )
        .component(
            sink.specification(id("capture"), json!({"path":capture_path}))?,
            sink,
        )
        .connect(
            edge("counter", "capture"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .bind_stream(endpoint("counter", "out"), stream("counter/out"))
        .build()?
        .configuration_snapshot()?
        .topology;
    assert!(
        !capture_path.exists(),
        "definition/export must not activate a sink"
    );
    let mut graph = desired.build(TopologyBindings {
        factories: registry.computation_factory_registry()?,
        ..TopologyBindings::default()
    })?;
    tokio::time::timeout(TIMEOUT, graph.start()?).await??;
    assert_eq!(values(&captured(&capture_path)?)?, [7, 8]);
    assert_eq!(
        configuration(&graph.configuration_snapshot()?, "capture")?,
        json!({"path":capture_path,"append":false})
    );

    let participants = registry.transactional_transformer_registry(Arc::new(
        drasi_core::middleware::MiddlewareTypeRegistry::new(),
    ))?;
    let mut transaction = TransactionTransformer::new(
        transaction_definition(&discovered)?,
        participants,
        storage(&directory.path().join("registered-transactions")),
    )
    .await?;
    transaction.start().await?;
    let output = transaction.transform(insert(1, 3)?).await?;
    assert_eq!(transaction_row(&output)?, (20, 1, 1));
    transaction.delivery_completed(&output).await?;
    transaction.stop().await?;
    Ok(())
}

struct HostCapture {
    descriptor: ComponentDescriptor,
    envelopes: Arc<Mutex<Vec<ChangeEnvelope>>>,
    stopped: Arc<AtomicBool>,
}
impl HostCapture {
    fn new(envelopes: Arc<Mutex<Vec<ChangeEnvelope>>>, stopped: Arc<AtomicBool>) -> Self {
        Self {
            descriptor: ComponentDescriptor::try_new(
                id("host"),
                vec![PortDescriptor::new(
                    PortId::try_new("in").expect("port"),
                    PortDirection::Input,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("host descriptor"),
            envelopes,
            stopped,
        }
    }
}
#[async_trait]
impl ComputationComponent for HostCapture {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<Value> {
        Ok(json!({"kind":"host-capture"}))
    }
    async fn start(&mut self) -> Result<()> {
        self.stopped.store(false, Ordering::Release);
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.stopped.store(true, Ordering::Release);
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for HostCapture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.envelopes
            .lock()
            .expect("capture lock")
            .push(input.envelope);
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn separately_loaded_pipeline_exact_outputs_configuration_and_topology_roundtrip(
) -> Result<()> {
    let plugin = plugin()?;
    assert_eq!(
        plugin.metadata().plugin.id.as_ref(),
        "drasi-computation-standard"
    );
    assert_eq!(plugin.metadata().abi_version, "1.0.0");
    assert_eq!(drasi_plugin_sdk::ffi::FFI_SDK_VERSION, "0.15.0");
    assert_eq!(plugin.factories().len(), 4);
    let source = factory(&plugin, COUNTER)?;
    let middleware = factory(&plugin, MIDDLEWARE)?;
    let sink = factory(&plugin, CAPTURE)?;
    let directory = directory()?;
    let path = directory.path().join("captured.jsonl");
    let source_config =
        json!({"stream":"counter/out","count":4,"start":2,"step":3,"interval_ms":1});
    let middleware_config = json!({"stream":"middleware/out","middleware":[{
        "name":"rename","kind":"relabel","config":{"labelMappings":{"Counter":"Projected"}}
    }],"pipeline":["rename"]});
    let host_outputs = Arc::new(Mutex::new(Vec::new()));
    let stopped = Arc::new(AtomicBool::new(false));
    let mut graph = ComputationGraph::builder("native-pipeline")
        .component(
            source.specification(id("counter"), source_config.clone())?,
            source,
        )
        .component(
            middleware.specification(id("middleware"), middleware_config.clone())?,
            middleware,
        )
        .component(
            sink.specification(id("capture"), json!({"path":path}))?,
            sink,
        )
        .sink(Box::new(HostCapture::new(
            host_outputs.clone(),
            stopped.clone(),
        )))
        .connect(
            edge("counter", "middleware"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("middleware", "capture"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("middleware", "host"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .bind_stream(endpoint("counter", "out"), stream("counter/out"))
        .bind_stream(endpoint("middleware", "out"), stream("middleware/out"))
        .build()?;
    let initial = graph.configuration_snapshot()?;
    assert!(matches!(
        &initial.configurations[&id("counter")],
        CapturedComponentConfiguration::Declared { .. }
    ));
    tokio::time::timeout(TIMEOUT, graph.start()?).await??;
    assert_eq!(graph.state(), GraphState::Completed);
    assert!(stopped.load(Ordering::Acquire));
    let output = captured(&path)?;
    assert_eq!(values(&output)?, [2, 5, 8, 11]);
    assert_eq!(
        values(&host_outputs.lock().expect("capture lock"))?,
        [2, 5, 8, 11]
    );
    for (index, envelope) in output.iter().enumerate() {
        assert_eq!(envelope.system().sequence(), index as u64 + 1);
        assert_eq!(envelope.system().stream(), &stream("middleware/out"));
        assert_eq!(
            row(envelope)?.get_metadata().labels.as_ref(),
            &[Arc::<str>::from("Projected")]
        );
        assert_eq!(
            envelope.lineage().context("lineage")?.system().stream(),
            &stream("counter/out")
        );
        let producer = GraphProducerProgress::from_envelope(envelope)?
            .context("volatile middleware identity")?;
        assert!(!producer.identity().persistent());
        assert!(envelope
            .annotations()
            .entries()
            .any(|entry| entry.key() == "drasi.standard.volatile-counter"
                && matches!(entry.value(), ContextValue::Bool(true))));
    }
    let snapshot = graph.configuration_snapshot()?;
    assert_eq!(
        configuration(&snapshot, "counter")?,
        json!({
            "stream":"counter/out","count":4,"start":2,"step":3,"interval_ms":1,"paused":false
        })
    );
    assert_eq!(configuration(&snapshot, "middleware")?, middleware_config);
    assert_eq!(
        configuration(&snapshot, "capture")?,
        json!({"path":path,"append":false})
    );
    let decoded: GraphConfigurationSnapshot =
        serde_json::from_slice(&serde_json::to_vec(&snapshot)?)?;
    let ComponentConstruction::Factory(original) = &decoded
        .topology
        .components
        .iter()
        .find(|component| component.descriptor.id() == &id("counter"))
        .context("counter definition")?
        .construction
    else {
        anyhow::bail!("native component lost its factory recipe");
    };
    assert_eq!(
        original.configuration.len(),
        5,
        "submitted recipe is not overwritten by normalized defaults"
    );
    assert_eq!(
        original
            .implementation
            .plugin
            .as_ref()
            .context("provenance")?
            .id
            .as_ref(),
        "drasi-computation-standard"
    );
    let mut bindings = TopologyBindings::default();
    plugin.register_factories(&mut bindings.factories)?;
    let restored_outputs = Arc::new(Mutex::new(Vec::new()));
    bindings.components.insert(
        "host".into(),
        ConstructedComponent::sink(Box::new(HostCapture::new(
            restored_outputs.clone(),
            Arc::new(AtomicBool::new(false)),
        ))),
    );
    let mut restored = decoded.topology.build(bindings)?;
    tokio::time::timeout(TIMEOUT, restored.start()?).await??;
    assert_eq!(values(&captured(&path)?)?, [2, 5, 8, 11]);
    assert_eq!(
        values(&restored_outputs.lock().expect("capture lock"))?,
        [2, 5, 8, 11]
    );
    assert_eq!(
        configuration(&restored.configuration_snapshot()?, "counter")?,
        configuration(&snapshot, "counter")?
    );
    Ok(())
}

fn stable_input(sequence: u64, change: SourceChange) -> Result<InputEnvelope> {
    // This host-owned input identity/sequence is stable across reconstruction.
    // Never use the volatile counter as a durable transaction replay source.
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_change(
            change,
            stream("stable-host/out"),
            sequence,
            None,
        )?,
    })
}
fn metadata() -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new("stable-host", "row"),
        labels: Arc::from([Arc::from("Counter")]),
        effective_from: 1,
    }
}
fn insert(sequence: u64, value: i64) -> Result<InputEnvelope> {
    stable_input(
        sequence,
        SourceChange::Insert {
            element: Element::Node {
                metadata: metadata(),
                properties: ElementPropertyMap::from(json!({"value":value})),
            },
        },
    )
}

#[tokio::test(flavor = "current_thread")]
async fn actual_library_rejects_bad_configuration_ports_schemas_and_failed_lifecycle() -> Result<()>
{
    let plugin = plugin()?;
    let source = factory(&plugin, COUNTER)?;
    for configuration in [
        json!({"stream":"counter/out","count":-1}),
        json!({"stream":"counter/out","count":2,"start":i64::MAX,"step":1}),
        json!({"stream":"counter/out","unknown":true}),
    ] {
        assert!(source
            .create_component(id("invalid"), configuration)
            .is_err());
    }
    let sink = factory(&plugin, CAPTURE)?;
    let directory = directory()?;
    let bad_path = directory.path().join("missing-parent/capture.jsonl");
    let mut failed = sink.create_component(id("failed"), json!({"path":bad_path}))?;
    assert!(!bad_path.exists(), "constructor must not open files");
    assert!(failed.start().await.is_err());
    assert_eq!(
        failed.configuration()?,
        json!({"path":bad_path,"append":false})
    );
    assert!(failed.handle(insert(1, 3)?).await.is_err());
    failed.stop().await?;
    let path = directory.path().join("sink.jsonl");
    let mut capture = sink.create_component(id("sink"), json!({"path":path}))?;
    assert!(capture.handle(insert(1, 3)?).await.is_err());
    capture.start().await?;
    let mut bad_port = insert(1, 3)?;
    bad_port.port = PortId::try_new("not-in")?;
    assert!(capture.handle(bad_port).await.is_err());
    let changes = ChangeSet::try_new(
        ChangeSetId::try_new("other", bytes::Bytes::from_static(b"other"))?,
        QueryChangeCodec::schema().descriptor().clone(),
        vec![],
    )?;
    let other = ChangeEnvelope::new(
        emission_id(&stream("other"), 1)?,
        changes,
        SystemMetadata::new(stream("other"), 1),
    );
    assert!(capture
        .handle(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: other
        })
        .await
        .is_err());
    capture.handle(insert(1, 3)?).await?;
    capture.stop().await?;
    assert!(capture.handle(insert(2, 4)?).await.is_err());
    assert_eq!(values(&captured(&path)?)?, [3]);
    let mut invalid = sink.specification(id("sink"), json!({"path":path}))?;
    invalid.configuration_version += 1;
    assert!(ComponentFactory::validate(sink.as_ref(), &invalid).is_err());
    let mut wrong_port = sink.specification(id("sink"), json!({"path":path}))?;
    wrong_port.descriptor = ComponentDescriptor::try_new(id("sink"), vec![])?;
    assert!(ComponentFactory::validate(sink.as_ref(), &wrong_port).is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn control_and_configuration_progress_while_native_next_is_pending_and_cancel_cleans_up(
) -> Result<()> {
    let plugin = plugin()?;
    for cancel in [false, true] {
        let source = factory(&plugin, COUNTER)?;
        let sink = factory(&plugin, CAPTURE)?;
        let directory = directory()?;
        let path = directory.path().join("controlled.jsonl");
        let mut graph = ComputationGraph::builder("controlled")
            .component(
                source.specification(
                    id("counter"),
                    json!({
                        "stream":"counter/out","count":2,"paused":true
                    }),
                )?,
                source,
            )
            .component(
                sink.specification(id("capture"), json!({"path":path}))?,
                sink,
            )
            .connect(
                edge("counter", "capture"),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .bind_stream(endpoint("counter", "out"), stream("counter/out"))
            .build()?;
        let run = graph.start()?;
        let control = run.control();
        let driver = async {
            let result = async {
                assert_eq!(
                    control.startup_report().await?.summary,
                    OperationSummary::Completed
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
                assert!(
                    captured(&path)?.is_empty(),
                    "paused counter must keep next pending"
                );
                let snapshot = control.configuration_snapshot()?;
                assert_eq!(
                    configuration(&snapshot, "counter")?["paused"],
                    true,
                    "snapshot must not attempt a getter through the busy data lease"
                );
                if cancel {
                    control.cancel();
                } else {
                    control
                        .component_handle(&id("capture"))?
                        .control()?
                        .notify_upstream(ControlNotification::Custom {
                            kind: "drasi.counter.resume".into(),
                            payload: Value::Null,
                        })?;
                }
                Result::<()>::Ok(())
            }
            .await;
            if result.is_err() {
                control.cancel();
            }
            result
        };
        let (outcome, driven) =
            tokio::time::timeout(TIMEOUT, async { tokio::join!(run, driver) }).await?;
        driven?;
        if cancel {
            assert!(
                matches!(outcome, Err(GraphError::Cancelled)),
                "cancellation must remain a failure: {outcome:?}"
            );
            assert_eq!(graph.state(), GraphState::Cancelled);
            assert!(captured(&path)?.is_empty());
            graph.shutdown().await?;
        } else {
            outcome?;
            assert_eq!(graph.state(), GraphState::Completed);
            assert_eq!(values(&captured(&path)?)?, [0, 1]);
        }
    }
    Ok(())
}

fn transaction_registry(plugin: &NativePlugin) -> Result<Arc<TransactionalTransformerRegistry>> {
    let factories = plugin.transactional_factories();
    assert_eq!(factories.len(), 1);
    assert_eq!(factories[0].implementation().name.as_ref(), ARITHMETIC);
    let mut registry = TransactionalTransformerRegistry::default();
    for factory in factories {
        registry.register(factory)?;
    }
    Ok(Arc::new(registry))
}
fn transaction_definition(plugin: &NativePlugin) -> Result<TransactionTransformerDefinition> {
    let implementation = factory(plugin, ARITHMETIC)?
        .metadata()
        .implementation
        .clone();
    Ok(TransactionTransformerDefinition {
        graph_id: "native-transactions".into(), id: id("transaction"), output_stream: stream("transaction/out"),
        steps: [
            ("first", json!({"stream":"first/out","add":2,"multiply":1,"counter_property":"first_count"})),
            ("second", json!({"stream":"second/out","add":0,"multiply":4,"counter_property":"second_count"})),
        ].into_iter().map(|(name, configuration)| TransactionStepDefinition {
            id: id(name), implementation: implementation.clone(), configuration_version: 1, configuration,
        }).collect(),
        outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
    })
}
fn storage(root: &Path) -> Arc<LegacyIndexProviderAdapter> {
    LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(root, false, false)))
}
async fn transaction(plugin: &NativePlugin, root: &Path) -> Result<TransactionTransformer> {
    let mut value = TransactionTransformer::new(
        transaction_definition(plugin)?,
        transaction_registry(plugin)?,
        storage(root),
    )
    .await?;
    value.start().await?;
    Ok(value)
}
fn transaction_row(output: &[OutputEnvelope]) -> Result<(i64, i64, i64)> {
    let [output] = output else {
        anyhow::bail!("expected exactly one transaction output");
    };
    let element = row(&output.envelope)?;
    Ok((
        integer(&element, "value")?,
        integer(&element, "first_count")?,
        integer(&element, "second_count")?,
    ))
}

#[tokio::test(flavor = "current_thread")]
async fn native_transaction_state_is_isolated_and_committed_output_replays_without_rerunning_steps(
) -> Result<()> {
    let plugin = plugin()?;
    let directory = directory()?;
    let root = directory.path().join("indexes");
    let original_input = insert(1, 3)?;
    let mut subject = transaction(&plugin, &root).await?;
    let original = subject.transform(original_input.clone()).await?;
    assert_eq!(transaction_row(&original)?, (20, 1, 1));
    assert!(GraphProducerProgress::from_envelope(&original[0].envelope)?
        .context("durable producer")?
        .identity()
        .persistent());
    subject.stop().await?;
    drop(subject);
    let mut subject = transaction(&plugin, &root).await?;
    assert!(subject.has_pending_emissions());
    let replay = subject.on_wakeup().await?;
    assert_eq!(transaction_row(&replay)?, (20, 1, 1));
    assert_eq!(
        replay[0].envelope.changes().id(),
        original[0].envelope.changes().id()
    );
    assert_eq!(
        replay[0].envelope.changes().operations(),
        original[0].envelope.changes().operations()
    );
    assert_eq!(
        GraphProducerProgress::from_envelope(&replay[0].envelope)?,
        GraphProducerProgress::from_envelope(&original[0].envelope)?
    );
    assert!(replay[0].envelope.system().sequence() > original[0].envelope.system().sequence());
    subject.delivery_completed(&replay).await?;
    let duplicate = subject.transform(original_input).await?;
    for output in &duplicate {
        assert_eq!(transaction_row(std::slice::from_ref(output))?, (20, 1, 1));
    }
    subject.delivery_completed(&duplicate).await?;
    let patch = stable_input(
        2,
        SourceChange::Update {
            element: Element::Node {
                metadata: metadata(),
                properties: ElementPropertyMap::from(json!({"tag":"updated"})),
            },
        },
    )?;
    let patched = subject.transform(patch).await?;
    assert_eq!(
        transaction_row(&patched)?,
        (20, 2, 2),
        "per-step raw element state supplies the patch's missing value"
    );
    subject.delivery_completed(&patched).await?;
    let deleted = subject
        .transform(stable_input(
            3,
            SourceChange::Delete {
                metadata: metadata(),
            },
        )?)
        .await?;
    assert!(matches!(
        GraphChangeCodec::decode_changes(&deleted[0].envelope)?.as_slice(),
        [SourceChange::Delete { .. }]
    ));
    subject.delivery_completed(&deleted).await?;
    let next = subject.transform(insert(4, 4)?).await?;
    assert_eq!(
        transaction_row(&next)?,
        (24, 4, 4),
        "replay and duplicate input must not increment either participant"
    );
    subject.delivery_completed(&next).await?;
    subject.stop().await?;
    drop(subject);
    let mut reconstructed = transaction(&plugin, &root).await?;
    assert!(!reconstructed.has_pending_emissions());
    let next = reconstructed.transform(insert(5, 5)?).await?;
    assert_eq!(transaction_row(&next)?, (28, 5, 5));
    reconstructed.delivery_completed(&next).await?;
    reconstructed.stop().await?;
    Ok(())
}

#[derive(Clone, Copy)]
enum Fault {
    Error,
    Pending,
}
struct Gate {
    mode: Fault,
    batch_writes: AtomicUsize,
    fired: AtomicBool,
    entered: Notify,
    release: Notify,
    active: AtomicUsize,
}
impl Gate {
    fn new(mode: Fault) -> Arc<Self> {
        Arc::new(Self {
            mode,
            batch_writes: AtomicUsize::new(0),
            fired: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
            active: AtomicUsize::new(0),
        })
    }
}
struct FaultProvider {
    inner: RocksDbIndexProvider,
    gate: Arc<Gate>,
}
#[async_trait]
impl IndexBackendPlugin for FaultProvider {
    async fn create_indexes(
        &self,
        query_id: &str,
    ) -> std::result::Result<CreatedIndexes, IndexError> {
        self.create_scoped_indexes(query_id, query_id).await
    }
    async fn create_scoped_indexes(
        &self,
        scope: &str,
        query_id: &str,
    ) -> std::result::Result<CreatedIndexes, IndexError> {
        let mut indexes = self.inner.create_scoped_indexes(scope, query_id).await?;
        indexes.set.element_index = Arc::new(FaultElements {
            inner: indexes.set.element_index,
            gate: self.gate.clone(),
        });
        Ok(indexes)
    }
    fn is_volatile(&self) -> bool {
        self.inner.is_volatile()
    }
    fn supports_atomic_query_output(&self) -> bool {
        self.inner.supports_atomic_query_output()
    }
}
struct FaultElements {
    inner: Arc<dyn ElementIndex>,
    gate: Arc<Gate>,
}
struct Active<'a>(&'a AtomicUsize);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
#[async_trait]
impl ElementIndex for FaultElements {
    async fn get_element(
        &self,
        reference: &ElementReference,
    ) -> std::result::Result<Option<Arc<Element>>, IndexError> {
        self.inner.get_element(reference).await
    }
    async fn set_element(
        &self,
        element: &Element,
        slots: &Vec<usize>,
    ) -> std::result::Result<(), IndexError> {
        if element.get_reference().element_id.as_ref() == "batches"
            && self.gate.batch_writes.fetch_add(1, Ordering::AcqRel) == 1
        {
            self.gate.active.fetch_add(1, Ordering::AcqRel);
            let _active = Active(&self.gate.active);
            self.inner.set_element(element, slots).await?;
            self.gate.fired.store(true, Ordering::Release);
            self.gate.entered.notify_one();
            match self.gate.mode {
                Fault::Error => {
                    return Err(IndexError::other(std::io::Error::other(
                        "injected failure after second step's staged write",
                    )))
                }
                Fault::Pending => self.gate.release.notified().await,
            }
            return Ok(());
        }
        self.inner.set_element(element, slots).await
    }
    async fn delete_element(
        &self,
        reference: &ElementReference,
    ) -> std::result::Result<(), IndexError> {
        self.inner.delete_element(reference).await
    }
    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> std::result::Result<Option<Arc<Element>>, IndexError> {
        self.inner.get_slot_element_by_ref(slot, reference).await
    }
    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> std::result::Result<ElementStream, IndexError> {
        self.inner
            .get_slot_elements_by_inbound(slot, reference)
            .await
    }
    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> std::result::Result<ElementStream, IndexError> {
        self.inner
            .get_slot_elements_by_outbound(slot, reference)
            .await
    }
    async fn clear(&self) -> std::result::Result<(), IndexError> {
        self.inner.clear().await
    }
    async fn set_joins(&self, path: &MatchPath, joins: &Vec<Arc<QueryJoin>>) {
        self.inner.set_joins(path, joins).await;
    }
}
async fn fault_transaction(
    plugin: &NativePlugin,
    root: &Path,
    gate: Arc<Gate>,
) -> Result<TransactionTransformer> {
    let provider = LegacyIndexProviderAdapter::new(Arc::new(FaultProvider {
        inner: RocksDbIndexProvider::new(root, false, false),
        gate,
    }));
    let mut subject = TransactionTransformer::new(
        transaction_definition(plugin)?,
        transaction_registry(plugin)?,
        provider,
    )
    .await?;
    subject.start().await?;
    Ok(subject)
}

#[tokio::test(flavor = "current_thread")]
async fn failed_foreign_state_rpc_rolls_back_every_step_and_preserves_exact_retry_input(
) -> Result<()> {
    let plugin = plugin()?;
    let directory = directory()?;
    let root = directory.path().join("indexes");
    let gate = Gate::new(Fault::Error);
    let input = insert(1, 3)?;
    let mut subject = fault_transaction(&plugin, &root, gate.clone()).await?;
    let error = subject
        .transform(input.clone())
        .await
        .expect_err("real state RPC failure must propagate");
    assert!(format!("{error:#}").contains("injected failure"));
    assert!(gate.fired.load(Ordering::Acquire));
    assert!(
        subject.transform(input.clone()).await.is_err(),
        "failed container requires reconstruction"
    );
    subject.stop().await?;
    drop(subject);
    let mut subject = transaction(&plugin, &root).await?;
    assert!(
        !subject.has_pending_emissions(),
        "failed input did not commit output"
    );
    let output = subject.transform(input).await?;
    assert_eq!(
        transaction_row(&output)?,
        (20, 1, 1),
        "both already-staged per-step counters rolled back"
    );
    subject.delivery_completed(&output).await?;
    subject.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_borrowed_transaction_rpc_revokes_plugin_work_and_rolls_back_real_storage(
) -> Result<()> {
    let plugin = plugin()?;
    let directory = directory()?;
    let root = directory.path().join("indexes");
    let gate = Gate::new(Fault::Pending);
    let input = insert(1, 3)?;
    let mut subject = fault_transaction(&plugin, &root, gate.clone()).await?;
    {
        let work = subject.transform(input.clone());
        tokio::pin!(work);
        tokio::time::timeout(TIMEOUT, async {
            tokio::select! {
                result = &mut work => anyhow::bail!("transaction completed before pending RPC: {result:?}"),
                _ = gate.entered.notified() => Ok(()),
            }
        }).await??;
        assert_eq!(gate.active.load(Ordering::Acquire), 1);
        // Dropping the host step cancels the foreign operation and revokes the
        // borrowed RPC scope. The storage adapter still owns any submitted I/O.
    }
    gate.release.notify_one();
    tokio::time::timeout(TIMEOUT, subject.stop()).await??;
    assert_eq!(
        gate.active.load(Ordering::Acquire),
        0,
        "storage cleanup drained submitted work"
    );
    assert_eq!(
        gate.batch_writes.load(Ordering::Acquire),
        2,
        "cancelled plugin did not continue processing"
    );
    drop(subject);
    let mut subject = transaction(&plugin, &root).await?;
    assert!(!subject.has_pending_emissions());
    let output = subject.transform(input).await?;
    assert_eq!(transaction_row(&output)?, (20, 1, 1));
    subject.delivery_completed(&output).await?;
    subject.stop().await?;
    Ok(())
}

struct StableSource {
    descriptor: ComponentDescriptor,
    input: Option<ChangeEnvelope>,
}
impl StableSource {
    fn new() -> Result<Self> {
        Ok(Self {
            descriptor: ComponentDescriptor::try_new(
                id("stable"),
                vec![PortDescriptor::new(
                    PortId::try_new("out")?,
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?,
            input: Some(insert(1, 3)?.envelope),
        })
    }
}
#[async_trait]
impl ComputationComponent for StableSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<Value> {
        Ok(
            json!({"kind":"host-owned-stable-input","stream":"stable-host/out","sequence":1,"value":3}),
        )
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for StableSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self.input.take().map(|envelope| OutputEnvelope {
            port: PortId::try_new("out").expect("port"),
            envelope,
        }))
    }
}

fn restore_indexes(recipe: &Value) -> Result<Arc<LegacyIndexProviderAdapter>> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Recipe {
        kind: String,
        path: PathBuf,
        enable_archive: bool,
        direct_io: bool,
    }
    let recipe: Recipe = serde_json::from_value(recipe.clone())?;
    anyhow::ensure!(recipe.kind == "rocksdbIndexes", "unknown index recipe");
    Ok(LegacyIndexProviderAdapter::new(Arc::new(
        RocksDbIndexProvider::new(recipe.path, recipe.enable_archive, recipe.direct_io),
    )))
}

async fn restore_retained(recipe: &Value) -> Result<ResourceHandle> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Recipe {
        kind: String,
        path: PathBuf,
        graph_id: String,
        component_id: String,
        key: String,
        capacity: NonZeroUsize,
        retention: RetentionPolicy,
        schemas: Vec<SchemaDescriptor>,
    }
    let recipe: Recipe = serde_json::from_value(recipe.clone())?;
    anyhow::ensure!(
        recipe.kind == "indexedEnvelopeStore",
        "unknown journal recipe"
    );
    anyhow::ensure!(
        recipe.schemas == vec![GraphChangeCodec::schema().descriptor().clone()],
        "this fixture's journal requires the exact graph-change schema"
    );
    let indexes = storage(&recipe.path)
        .create_indexes(&recipe.graph_id, &recipe.component_id)
        .await?;
    let store = Arc::new(IndexedEnvelopeStore::try_new(
        indexes,
        Arc::new(codec()?),
        recipe.key,
        recipe.capacity,
        recipe.retention,
    )?);
    let resource = Arc::new(RetainedStoreResource(store));
    Ok(ResourceHandle::new(ResourceRole::StateStore, resource.clone()).with_cleanup(resource))
}

#[tokio::test(flavor = "current_thread")]
async fn persistent_resource_recipe_and_native_transaction_configuration_roundtrip() -> Result<()> {
    let plugin = plugin()?;
    let directory = directory()?;
    let root = directory.path().join("indexes");
    let registry = transaction_registry(&plugin)?;
    let definition = transaction_definition(&plugin)?;
    let indexes = ResourceId::try_new("indexes")?;
    let participants = ResourceId::try_new("participants")?;
    let retained = ResourceId::try_new("delivery")?;
    let capture = factory(&plugin, CAPTURE)?;
    let output_path = directory.path().join("durable-output.jsonl");
    let journal_root = directory.path().join("journal");
    let recipe =
        json!({"kind":"rocksdbIndexes","path":root,"enable_archive":false,"direct_io":false});
    let journal_recipe = json!({"kind":"indexedEnvelopeStore","path":journal_root,"graph_id":"native-transactions",
        "component_id":"delivery","key":"delivery","capacity":8,"retention":"Backpressure",
        "schemas":[GraphChangeCodec::schema().descriptor()]});
    let specification =
        definition.specification(&registry, participants.clone(), indexes.clone())?;
    let mut graph = ComputationGraph::builder(definition.graph_id.as_str())
        .declare_resource(ResourceSpecification {
            id: indexes.clone(),
            role: ResourceRole::IndexBackend,
            ownership: ResourceOwnership::Graph,
            binding: "rocksdb".into(),
        })?
        .resource_configuration(indexes.clone(), recipe.clone())?
        .provide_resource(indexes.clone(), restore_indexes(&recipe)?.resource())?
        .declare_resource(ResourceSpecification {
            id: participants.clone(),
            role: ResourceRole::Component,
            ownership: ResourceOwnership::Borrowed,
            binding: "native-participants".into(),
        })?
        .provide_resource(
            participants.clone(),
            ResourceHandle::new(
                ResourceRole::Component,
                Arc::new(TransactionalTransformerRegistryResource(registry.clone())),
            ),
        )?
        .declare_resource(ResourceSpecification {
            id: retained.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: "delivery".into(),
        })?
        .resource_configuration(retained.clone(), journal_recipe.clone())?
        .provide_resource(retained.clone(), restore_retained(&journal_recipe).await?)?
        .source(Box::new(StableSource::new()?))
        .component(
            specification,
            Arc::new(TransactionTransformerFactory::default()),
        )
        .component(
            capture.specification(id("capture"), json!({"path":output_path,"append":true}))?,
            capture,
        )
        .connect(
            edge("stable", "transaction"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("transaction", "capture"),
            Box::new(RetainedPipeConfig {
                resource: retained.clone(),
                capacity: NonZeroUsize::new(8).expect("capacity"),
                durable: true,
                retention: RetentionPolicy::Backpressure,
                gap_policy: ReplayGapPolicy::Strict,
            }),
        )
        .bind_stream(endpoint("stable", "out"), stream("stable-host/out"))
        .bind_stream(endpoint("transaction", "out"), stream("transaction/out"))
        .build()?;
    tokio::time::timeout(TIMEOUT, graph.start()?).await??;
    assert_eq!(values(&captured(&output_path)?)?, [20]);
    let snapshot = graph.configuration_snapshot()?;
    assert_eq!(snapshot.topology.resource_configurations[&indexes], recipe);
    assert_eq!(
        configuration(&snapshot, "transaction")?,
        json!({"transaction":definition})
    );
    let decoded: GraphConfigurationSnapshot =
        serde_json::from_slice(&serde_json::to_vec(&snapshot)?)?;
    graph.dispose().await?;
    drop(graph);
    let mut bindings = TopologyBindings {
        factories: FactoryRegistry::standard(),
        ..TopologyBindings::default()
    };
    plugin.register_factories(&mut bindings.factories)?;
    bindings.resources.insert(
        indexes.clone(),
        restore_indexes(&decoded.topology.resource_configurations[&indexes])?.resource(),
    );
    bindings.resources.insert(
        participants,
        ResourceHandle::new(
            ResourceRole::Component,
            Arc::new(TransactionalTransformerRegistryResource(registry)),
        ),
    );
    bindings.resources.insert(
        retained.clone(),
        restore_retained(&decoded.topology.resource_configurations[&retained]).await?,
    );
    bindings.components.insert(
        "stable".into(),
        ConstructedComponent::source(Box::new(StableSource::new()?)),
    );
    let mut restored = decoded.topology.build(bindings)?;
    tokio::time::timeout(TIMEOUT, restored.start()?).await??;
    let restored_snapshot = restored.configuration_snapshot()?;
    assert_eq!(
        restored_snapshot.topology.resource_configurations[&indexes],
        recipe
    );
    assert_eq!(
        restored_snapshot.topology.resource_configurations[&retained],
        journal_recipe
    );
    assert_eq!(
        configuration(&restored_snapshot, "transaction")?,
        configuration(&snapshot, "transaction")?
    );
    let replayed = captured(&output_path)?;
    assert_eq!(
        values(&replayed)?,
        [20, 20],
        "retained output is redelivered; capture does not claim exactly-once effects"
    );
    assert_eq!(replayed[0].changes().id(), replayed[1].changes().id());
    assert_eq!(
        GraphProducerProgress::from_envelope(&replayed[0])?,
        GraphProducerProgress::from_envelope(&replayed[1])?
    );
    for output in &replayed {
        assert_eq!(integer(&row(output)?, "first_count")?, 1);
        assert_eq!(
            integer(&row(output)?, "second_count")?,
            1,
            "redelivery must not re-execute either participant"
        );
    }
    restored.dispose().await?;
    Ok(())
}
