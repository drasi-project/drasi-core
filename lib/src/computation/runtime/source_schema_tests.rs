// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::schema::{NodeSchema, SourceSchema};

#[derive(Debug, thiserror::Error)]
#[error("native catalog discovery failed")]
struct SchemaReadFailure;

#[derive(Default)]
struct Probe {
    calls: AtomicUsize,
    blocked: AtomicBool,
    entered: Notify,
    release: Notify,
    next_entered: Notify,
    fail: AtomicBool,
}

struct Reader {
    probe: Arc<Probe>,
    schema: Option<SourceSchema>,
}

#[async_trait]
impl SourceSchemaReader for Reader {
    async fn describe_schema(&self) -> anyhow::Result<Option<SourceSchema>> {
        self.probe.calls.fetch_add(1, Ordering::AcqRel);
        self.probe.entered.notify_one();
        if self.probe.blocked.load(Ordering::Acquire) {
            self.probe.release.notified().await;
        }
        if self.probe.fail.load(Ordering::Acquire) {
            return Err(SchemaReadFailure.into());
        }
        Ok(self.schema.clone())
    }
}

struct NativeSource {
    descriptor: ComponentDescriptor,
    reader: Option<Arc<dyn SourceSchemaReader>>,
    probe: Arc<Probe>,
}

#[async_trait]
impl ComputationComponent for NativeSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        self.reader.clone()
    }
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({"nodes": [{"label": "ConfigurationOnly"}]}))
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for NativeSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        self.probe.next_entered.notify_one();
        std::future::pending().await
    }
}

struct Sink(ComponentDescriptor);

#[async_trait]
impl ComputationComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}

fn schema(label: &str) -> SourceSchema {
    SourceSchema {
        nodes: vec![NodeSchema::new(label)],
        relations: Vec::new(),
    }
}

fn descriptor(id: &str, output: bool) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        ComponentId::try_new(id).unwrap(),
        vec![PortDescriptor::new(
            PortId::try_new(if output { "out" } else { "in" }).unwrap(),
            if output {
                PortDirection::Output
            } else {
                PortDirection::Input
            },
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
    .unwrap()
}

fn source(
    id: &str,
    schema: Option<Option<SourceSchema>>,
) -> (
    NativeSource,
    Arc<Probe>,
    Option<Weak<dyn SourceSchemaReader>>,
) {
    let probe = Arc::new(Probe::default());
    let reader = schema.map(|schema| {
        Arc::new(Reader {
            probe: probe.clone(),
            schema,
        }) as Arc<dyn SourceSchemaReader>
    });
    let weak = reader.as_ref().map(Arc::downgrade);
    (
        NativeSource {
            descriptor: descriptor(id, true),
            reader,
            probe: probe.clone(),
        },
        probe,
        weak,
    )
}

async fn add(core: &DrasiLib, source: NativeSource) -> ComponentHandle {
    let id = source.descriptor.id().to_string();
    core.add_computation_component(
        ComponentAddition::new(ConstructedComponent::source(Box::new(source)))
            .auto_start(false)
            .bind_stream(
                PortId::try_new("out").unwrap(),
                StreamId::try_new(format!("{id}/out")).unwrap(),
            ),
    )
    .await
    .unwrap()
}

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .expect("source inspection and lifecycle must remain independently responsive")
}

struct Fixture {
    core: Arc<DrasiLib>,
    probe: Arc<Probe>,
    reader: Weak<dyn SourceSchemaReader>,
    handle: ComponentHandle,
}

async fn fixture() -> Fixture {
    let (legacy, legacy_control) = ControlledSource::new("legacy");
    *legacy_control.schema.lock().unwrap() = Some(schema("Legacy"));
    let core = Arc::new(builder().with_source(legacy).build().await.unwrap());
    // Initialization reports resources, which advance the topology revision.
    bounded(core.computation_component("legacy").unwrap().wait_created())
        .await
        .unwrap();
    let sink = core
        .add_computation_component(
            ComponentAddition::new(ConstructedComponent::sink(Box::new(Sink(descriptor(
                "sink", false,
            )))))
            .auto_start(false),
        )
        .await
        .unwrap();
    let (native, probe, reader) = source("native", Some(Some(schema("Native"))));
    let handle = add(&core, native).await;
    let control = core.computation_control().unwrap();
    let plan = control
        .preview(
            control.desired_snapshot().revision,
            vec![DesiredMutation::Bind(DesiredRelationship {
                definition: EdgeDefinition {
                    from: Endpoint::new(handle.id().clone(), PortId::try_new("out").unwrap()),
                    to: Endpoint::new(sink.id().clone(), PortId::try_new("in").unwrap()),
                },
                policy: Default::default(),
                pipe: DesiredPipe::Bounded { capacity: 2 },
            })],
        )
        .await
        .unwrap();
    assert_eq!(
        control
            .reconcile(plan, TopologyBindings::default())
            .await
            .unwrap()
            .summary,
        OperationSummary::Completed
    );
    bounded(handle.wait_created()).await.unwrap();
    assert_eq!(probe.calls.load(Ordering::Acquire), 0);
    bounded(sink.start()).await.unwrap();
    bounded(handle.start()).await.unwrap();
    bounded(probe.next_entered.notified()).await;
    Fixture {
        core,
        probe,
        reader: reader.unwrap(),
        handle,
    }
}

async fn discovery() {
    let Fixture { core, probe, .. } = fixture().await;
    assert_eq!(
        bounded(core.get_source_schema("native")).await.unwrap(),
        Some(schema("Native"))
    );
    assert_eq!(
        core.get_source_schema("legacy").await.unwrap(),
        Some(schema("Legacy"))
    );
    for (id, label) in [("native", "Native"), ("legacy", "Legacy")] {
        assert_eq!(
            bounded(core.source_manager().get_source_schema(id.into()))
                .await
                .unwrap(),
            Some(schema(label))
        );
    }
    assert!(core
        .list_sources()
        .await
        .unwrap()
        .iter()
        .any(|(id, status)| id == "native" && *status == ComponentStatus::Running));
    assert!(core.source_instance("native").await.is_err());
    assert_eq!(core.get_source_info("native").await.unwrap().id, "native");
    for id in ["missing", "sink"] {
        assert!(matches!(
            core.get_source_schema(id)
                .await
                .unwrap_err()
                .classification(),
            crate::DrasiError::ComponentNotFound { .. }
        ));
        assert!(core
            .source_manager()
            .get_source_schema(id.into())
            .await
            .unwrap_err()
            .downcast_ref::<crate::error::ComponentNotFoundError>()
            .is_some());
    }
    for (id, capability) in [("absent", None), ("unsupported", Some(None))] {
        let (native, _, _) = source(id, capability);
        add(&core, native).await;
        assert_eq!(core.get_source_schema(id).await.unwrap(), None);
        assert_eq!(
            core.source_manager()
                .get_source_schema(id.into())
                .await
                .unwrap(),
            None
        );
    }
    let graph = bounded(core.get_graph_schema()).await.unwrap();
    assert!(graph.nodes["Native"].sources.contains("native"));
    assert!(graph.nodes["Legacy"].sources.contains("legacy"));
    assert!(!graph.nodes.contains_key("ConfigurationOnly"));
    assert!(graph.sources_without_schema.contains("absent"));
    assert!(graph.sources_without_schema.contains("unsupported"));
    probe.fail.store(true, Ordering::Release);
    assert!(bounded(core.get_source_schema("native"))
        .await
        .unwrap_err()
        .downcast_ref::<SchemaReadFailure>()
        .is_some());
    assert!(bounded(core.get_graph_schema())
        .await
        .unwrap_err()
        .downcast_ref::<SchemaReadFailure>()
        .is_some());
    assert!(
        bounded(core.source_manager().get_source_schema("native".into()))
            .await
            .unwrap_err()
            .downcast_ref::<SchemaReadFailure>()
            .is_some()
    );
    core.shutdown().await.unwrap();
}

async fn pending_read(
    fixture: &Fixture,
) -> tokio::task::JoinHandle<crate::Result<Option<SourceSchema>>> {
    fixture.probe.blocked.store(true, Ordering::Release);
    let read = tokio::spawn({
        let core = fixture.core.clone();
        async move { core.get_source_schema("native").await }
    });
    bounded(fixture.probe.entered.notified()).await;
    read
}

async fn stale_read(read: tokio::task::JoinHandle<crate::Result<Option<SourceSchema>>>) {
    assert!(matches!(
        bounded(read)
            .await
            .unwrap()
            .unwrap_err()
            .downcast_ref::<GraphError>(),
        Some(GraphError::StaleGeneration)
    ));
}

async fn stop_fences_reads() {
    let fixture = fixture().await;
    let read = pending_read(&fixture).await;
    bounded(fixture.core.stop_source("native")).await.unwrap();
    stale_read(read).await;
    fixture.probe.blocked.store(false, Ordering::Release);
    bounded(fixture.core.start_source("native")).await.unwrap();
    bounded(fixture.probe.next_entered.notified()).await;
    assert_eq!(
        bounded(fixture.core.get_source_schema("native"))
            .await
            .unwrap(),
        Some(schema("Native"))
    );
    fixture.core.shutdown().await.unwrap();
}

async fn remove_fences_reads() {
    let fixture = fixture().await;
    let publication = fixture
        .core
        .computation_control()
        .unwrap()
        .registry_snapshot();
    let read = pending_read(&fixture).await;
    let control = fixture.core.computation_control().unwrap();
    let plan = control
        .preview(
            control.desired_snapshot().revision,
            vec![DesiredMutation::RemoveComponents {
                selection: GraphSelection::Exact(vec![fixture.handle.id().clone()]),
                policy: RemovalPolicy::Cascade,
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        bounded(control.reconcile(plan, TopologyBindings::default()))
            .await
            .unwrap()
            .summary,
        OperationSummary::Completed
    );
    stale_read(read).await;
    assert!(fixture.reader.upgrade().is_none());
    assert!(matches!(
        publication.source_schema_reader(fixture.handle.id()),
        Err(GraphError::StaleGeneration)
    ));
    let (replacement, _, _) = source("native", Some(Some(schema("Replacement"))));
    add(&fixture.core, replacement).await;
    assert_eq!(
        fixture.core.get_source_schema("native").await.unwrap(),
        Some(schema("Replacement"))
    );
    fixture.core.shutdown().await.unwrap();
}

async fn replacement_fences_reads() {
    let fixture = fixture().await;
    let read = pending_read(&fixture).await;
    let control = fixture.core.computation_control().unwrap();
    let publication = control.registry_snapshot();
    let desired = control.desired_snapshot();
    let replacement = desired
        .select(GraphSelection::Exact(vec![fixture.handle.id().clone()]))
        .unwrap()
        .components
        .remove(0);
    let ComponentConstruction::External { binding } = &replacement.construction else {
        panic!("native external source")
    };
    let (native, _, _) = source("native", Some(Some(schema("Replacement"))));
    let mut bindings = TopologyBindings::default();
    bindings.components.insert(
        binding.clone(),
        ConstructedComponent::source(Box::new(native)),
    );
    let plan = control
        .preview(
            desired.revision,
            vec![DesiredMutation::ReplaceComponent(replacement)],
        )
        .await
        .unwrap();
    assert_eq!(
        bounded(control.reconcile(plan, bindings))
            .await
            .unwrap()
            .summary,
        OperationSummary::Completed
    );
    stale_read(read).await;
    assert!(fixture.reader.upgrade().is_none());
    assert!(matches!(
        publication.source_schema_reader(fixture.handle.id()),
        Err(GraphError::StaleGeneration)
    ));
    assert_ne!(
        fixture
            .core
            .computation_component("native")
            .unwrap()
            .generation(),
        fixture.handle.generation()
    );
    assert_eq!(
        bounded(fixture.core.get_source_schema("native"))
            .await
            .unwrap(),
        Some(schema("Replacement"))
    );
    fixture.core.shutdown().await.unwrap();
}

async fn aggregate_fences_earlier_reads() {
    let fixture = fixture().await;
    let (later, probe, _) = source("zz-later", Some(Some(schema("Later"))));
    probe.blocked.store(true, Ordering::Release);
    add(&fixture.core, later).await;
    let read = tokio::spawn({
        let core = fixture.core.clone();
        async move { core.get_graph_schema().await }
    });
    bounded(probe.entered.notified()).await;
    assert_eq!(fixture.probe.calls.load(Ordering::Acquire), 1);
    bounded(fixture.core.stop_source("native")).await.unwrap();
    probe.release.notify_one();
    assert!(matches!(
        bounded(read)
            .await
            .unwrap()
            .unwrap_err()
            .downcast_ref::<GraphError>(),
        Some(GraphError::StaleGeneration)
    ));
    fixture.core.shutdown().await.unwrap();
}

async fn shutdown_fences_reads() {
    let fixture = fixture().await;
    let read = pending_read(&fixture).await;
    bounded(fixture.core.shutdown()).await.unwrap();
    stale_read(read).await;
    assert!(fixture.reader.upgrade().is_none());
}

async fn source_manager_stop_fences_reads() {
    let fixture = fixture().await;
    fixture.probe.blocked.store(true, Ordering::Release);
    let read = tokio::spawn({
        let core = fixture.core.clone();
        async move {
            core.source_manager()
                .get_source_schema("native".into())
                .await
        }
    });
    bounded(fixture.probe.entered.notified()).await;
    bounded(fixture.core.stop_source("native")).await.unwrap();
    assert!(matches!(
        bounded(read)
            .await
            .unwrap()
            .unwrap_err()
            .downcast_ref::<GraphError>(),
        Some(GraphError::StaleGeneration)
    ));
    fixture.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn source_schema_manager_stop_current_thread() {
    source_manager_stop_fences_reads().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_manager_stop_multi_thread() {
    source_manager_stop_fences_reads().await;
}

#[tokio::test]
async fn source_schema_discovery_current_thread() {
    discovery().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_discovery_multi_thread() {
    discovery().await;
}

#[tokio::test]
async fn source_schema_stop_current_thread() {
    stop_fences_reads().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_stop_multi_thread() {
    stop_fences_reads().await;
}

#[tokio::test]
async fn source_schema_removal_current_thread() {
    remove_fences_reads().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_removal_multi_thread() {
    remove_fences_reads().await;
}

#[tokio::test]
async fn source_schema_replacement_current_thread() {
    replacement_fences_reads().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_replacement_multi_thread() {
    replacement_fences_reads().await;
}

#[tokio::test]
async fn source_schema_aggregate_fence_current_thread() {
    aggregate_fences_earlier_reads().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_aggregate_fence_multi_thread() {
    aggregate_fences_earlier_reads().await;
}

#[tokio::test]
async fn source_schema_shutdown_current_thread() {
    shutdown_fences_reads().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_shutdown_multi_thread() {
    shutdown_fences_reads().await;
}
