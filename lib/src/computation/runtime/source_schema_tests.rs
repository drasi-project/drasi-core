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
    initializing: Notify,
    release_initialization: Notify,
    hold_initialization: AtomicBool,
    fail_initialization: AtomicBool,
    constructing: Notify,
    release_construction: Notify,
}

#[derive(Debug, thiserror::Error)]
#[error("schema fixture initialization failed")]
struct SchemaInitializationFailure;

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
    async fn initialize(&mut self) -> anyhow::Result<()> {
        if self.probe.hold_initialization.load(Ordering::Acquire) {
            self.probe.initializing.notify_one();
            self.probe.release_initialization.notified().await;
        }
        if self.probe.fail_initialization.load(Ordering::Acquire) {
            return Err(SchemaInitializationFailure.into());
        }
        Ok(())
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

fn descriptor(id: ComponentId, output: bool) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        id,
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
    id: ComponentId,
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

async fn add(core: &DrasiLib, name: ComponentName, source: NativeSource) -> ComponentHandle {
    let id = source.descriptor.id().to_string();
    let mut registry = core.computation_control().unwrap().subscribe_registry();
    let handle = core
        .add_computation_component(
            ComponentAddition::new(name.clone(), ConstructedComponent::source(Box::new(source)))
                .auto_start(false)
                .bind_stream(
                    PortId::try_new("out").unwrap(),
                    StreamId::try_new(format!("{id}/out")).unwrap(),
                ),
        )
        .await
        .unwrap();
    bounded(registry.wait_for(|snapshot| {
        snapshot
            .desired
            .component_by_name(&name)
            .is_some_and(|node| node.descriptor.id() == handle.id())
    }))
    .await
    .unwrap();
    handle
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
    let (legacy, legacy_control) = ControlledSource::fixture(
        drasi_core_types::ComponentName::try_new("legacy").expect("fixture component name"),
    );
    *legacy_control.schema.lock().unwrap() = Some(schema("Legacy"));
    let core = Arc::new(
        builder()
            .with_allow_external_component_ids(true)
            .with_source_fixture(legacy)
            .build()
            .await
            .unwrap(),
    );
    // Initialization reports resources, which advance the topology revision.
    bounded(
        core.component_by_name(
            &drasi_core_types::ComponentName::try_new("legacy").expect("fixture component name"),
        )
        .unwrap()
        .wait_created(),
    )
    .await
    .unwrap();
    let sink = core
        .add_computation_component(
            ComponentAddition::new(
                ComponentName::try_new("sink").unwrap(),
                ConstructedComponent::sink(Box::new(Sink(descriptor(
                    crate::test_helpers::component_id(100),
                    false,
                )))),
            )
            .auto_start(false),
        )
        .await
        .unwrap();
    let (native, probe, reader) = source(
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d2")
            .expect("fixture UUID"),
        Some(Some(schema("Native"))),
    );
    let handle = add(&core, ComponentName::try_new("native").unwrap(), native).await;
    let report = bounded(apply_native_topology(&core, |topology| {
        topology.relationships.push(DesiredRelationship {
            definition: EdgeDefinition {
                from: Endpoint::new(handle.id().clone(), PortId::try_new("out").unwrap()),
                to: Endpoint::new(sink.id().clone(), PortId::try_new("in").unwrap()),
            },
            policy: Default::default(),
            pipe: DesiredPipe::Bounded { capacity: 2 },
        });
    }))
    .await
    .unwrap();
    assert!(report.applied && report.error.is_none(), "{report:?}");
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
        bounded(core.get_source_schema_by_name(
            &drasi_core_types::ComponentName::try_new("native").expect("fixture component name")
        ))
        .await
        .unwrap(),
        Some(schema("Native"))
    );
    assert_eq!(
        core.get_source_schema_by_name(
            &drasi_core_types::ComponentName::try_new("legacy").expect("fixture component name")
        )
        .await
        .unwrap(),
        Some(schema("Legacy"))
    );
    for (id, label) in [("native", "Native"), ("legacy", "Legacy")] {
        let name = ComponentName::try_new(id).expect("fixture component name");
        assert_eq!(
            bounded(core.source_manager().get_source_schema_by_name(&name))
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
        .any(
            |entry| entry.name == crate::ComponentName::try_new("native").unwrap()
                && entry.status == ComponentStatus::Running
        ));
    assert!(core
        .source_instance(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d2")
                .expect("fixture UUID")
        )
        .await
        .is_err());
    assert_eq!(
        core.get_source_info_by_name(
            &drasi_core_types::ComponentName::try_new("native").expect("fixture component name")
        )
        .await
        .unwrap()
        .id,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d2")
            .expect("fixture UUID")
    );
    for id in ["missing", "sink"] {
        let name = ComponentName::try_new(id).expect("fixture component name");
        assert!(matches!(
            core.get_source_schema_by_name(&name)
                .await
                .unwrap_err()
                .classification(),
            crate::DrasiError::ComponentNotFound { .. }
        ));
        assert!(core
            .source_manager()
            .get_source_schema_by_name(&name)
            .await
            .unwrap_err()
            .downcast_ref::<crate::DrasiError>()
            .is_some_and(|error| matches!(
                error.classification(),
                crate::DrasiError::ComponentNotFound { .. }
            )));
    }
    for (name, id, capability) in [
        ("absent", crate::test_helpers::component_id(467), None),
        (
            "unsupported",
            crate::test_helpers::component_id(468),
            Some(None),
        ),
    ] {
        let component_name = ComponentName::try_new(name).expect("fixture component name");
        let (native, _, _) = source(id, capability);
        add(&core, component_name.clone(), native).await;
        assert_eq!(
            core.get_source_schema_by_name(&component_name)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            core.source_manager()
                .get_source_schema_by_name(&component_name)
                .await
                .unwrap(),
            None
        );
    }
    let graph = bounded(core.get_graph_schema()).await.unwrap();
    assert!(graph.nodes["Native"].sources.contains(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d2")
            .expect("fixture UUID")
    ));
    assert!(graph.nodes["Legacy"].sources.contains(
        core.component_by_name(&ComponentName::try_new("legacy").unwrap())
            .unwrap()
            .id()
    ));
    assert!(!graph.nodes.contains_key("ConfigurationOnly"));
    assert!(graph.sources_without_schema.contains(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d3")
            .expect("fixture UUID")
    ));
    assert!(graph.sources_without_schema.contains(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d4")
            .expect("fixture UUID")
    ));
    probe.fail.store(true, Ordering::Release);
    assert!(bounded(core.get_source_schema_by_name(
        &drasi_core_types::ComponentName::try_new("native").expect("fixture component name")
    ))
    .await
    .unwrap_err()
    .downcast_ref::<SchemaReadFailure>()
    .is_some());
    assert!(bounded(core.get_graph_schema())
        .await
        .unwrap_err()
        .downcast_ref::<SchemaReadFailure>()
        .is_some());
    assert!(bounded(core.source_manager().get_source_schema_by_name(
        &ComponentName::try_new("native").expect("fixture component name")
    ))
    .await
    .unwrap_err()
    .downcast_ref::<SchemaReadFailure>()
    .is_some());
    core.shutdown().await.unwrap();
}

async fn pending_read(
    fixture: &Fixture,
) -> tokio::task::JoinHandle<crate::Result<Option<SourceSchema>>> {
    fixture.probe.blocked.store(true, Ordering::Release);
    let read = tokio::spawn({
        let core = fixture.core.clone();
        async move {
            core.get_source_schema_by_name(
                &drasi_core_types::ComponentName::try_new("native")
                    .expect("fixture component name"),
            )
            .await
        }
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
    bounded(fixture.core.stop_source_by_name(
        &drasi_core_types::ComponentName::try_new("native").expect("fixture component name"),
    ))
    .await
    .unwrap();
    stale_read(read).await;
    fixture.probe.blocked.store(false, Ordering::Release);
    bounded(fixture.core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("native").expect("fixture component name"),
    ))
    .await
    .unwrap();
    bounded(fixture.probe.next_entered.notified()).await;
    assert_eq!(
        bounded(fixture.core.get_source_schema_by_name(
            &drasi_core_types::ComponentName::try_new("native").expect("fixture component name")
        ))
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
    let report = bounded(apply_native_topology(&fixture.core, |topology| {
        remove_native_definition(topology, fixture.handle.id());
    }))
    .await
    .unwrap();
    assert!(report.applied && report.error.is_none(), "{report:?}");
    stale_read(read).await;
    assert!(fixture.reader.upgrade().is_none());
    assert!(matches!(
        publication.source_schema_reader(fixture.handle.id()),
        Err(GraphError::StaleGeneration)
    ));
    let (replacement, _, _) = source(
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d2")
            .expect("fixture UUID"),
        Some(Some(schema("Replacement"))),
    );
    add(
        &fixture.core,
        ComponentName::try_new("native").unwrap(),
        replacement,
    )
    .await;
    assert_eq!(
        fixture
            .core
            .get_source_schema_by_name(
                &drasi_core_types::ComponentName::try_new("native")
                    .expect("fixture component name")
            )
            .await
            .unwrap(),
        Some(schema("Replacement"))
    );
    fixture.core.shutdown().await.unwrap();
}

async fn replacement_fences_reads() {
    let fixture = fixture().await;
    let read = pending_read(&fixture).await;
    let control = fixture.core.computation_control().unwrap();
    let publication = control.registry_snapshot();
    let report = bounded(apply_native_topology(&fixture.core, |topology| {
        remove_native_definition(topology, fixture.handle.id());
    }))
    .await
    .unwrap();
    assert!(report.applied && report.error.is_none(), "{report:?}");
    stale_read(read).await;
    assert!(fixture.reader.upgrade().is_none());
    let (native, _, _) = source(
        fixture.handle.id().clone(),
        Some(Some(schema("Replacement"))),
    );
    add(
        &fixture.core,
        ComponentName::try_new("native").unwrap(),
        native,
    )
    .await;
    assert!(matches!(
        publication.source_schema_reader(fixture.handle.id()),
        Err(GraphError::StaleGeneration)
    ));
    assert!(matches!(
        fixture.handle.observed(),
        Err(GraphError::StaleGeneration)
    ));
    assert_ne!(
        fixture
            .core
            .component_by_name(
                &drasi_core_types::ComponentName::try_new("native")
                    .expect("fixture component name")
            )
            .unwrap()
            .generation(),
        fixture.handle.generation()
    );
    assert_eq!(
        bounded(fixture.core.get_source_schema_by_name(
            &drasi_core_types::ComponentName::try_new("native").expect("fixture component name")
        ))
        .await
        .unwrap(),
        Some(schema("Replacement"))
    );
    fixture.core.shutdown().await.unwrap();
}

async fn aggregate_fences_earlier_reads() {
    let fixture = fixture().await;
    let (later, probe, _) = source(
        crate::test_helpers::component_id(469),
        Some(Some(schema("Later"))),
    );
    probe.blocked.store(true, Ordering::Release);
    add(
        &fixture.core,
        ComponentName::try_new("zz-later").unwrap(),
        later,
    )
    .await;
    let read = tokio::spawn({
        let core = fixture.core.clone();
        async move { core.get_graph_schema().await }
    });
    bounded(probe.entered.notified()).await;
    assert_eq!(fixture.probe.calls.load(Ordering::Acquire), 1);
    bounded(fixture.core.stop_source_by_name(
        &drasi_core_types::ComponentName::try_new("native").expect("fixture component name"),
    ))
    .await
    .unwrap();
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
                .get_source_schema_by_name(
                    &ComponentName::try_new("native").expect("fixture component name"),
                )
                .await
        }
    });
    bounded(fixture.probe.entered.notified()).await;
    bounded(fixture.core.stop_source_by_name(
        &drasi_core_types::ComponentName::try_new("native").expect("fixture component name"),
    ))
    .await
    .unwrap();
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

async fn schema_probe_bounded<T>(
    future: impl std::future::Future<Output = T>,
) -> anyhow::Result<T> {
    Ok(tokio::time::timeout(Duration::from_secs(3), future).await?)
}

fn release_schema_probe(probe: &Probe) {
    probe.release.notify_one();
    probe.release_initialization.notify_one();
    probe.release_construction.notify_one();
}

async fn add_schema_probe(
    core: &DrasiLib,
    name: ComponentName,
    source: NativeSource,
) -> anyhow::Result<ComponentHandle> {
    let id = source.descriptor.id().to_string();
    core.add_computation_component(
        ComponentAddition::new(name, ConstructedComponent::source(Box::new(source)))
            .auto_start(false)
            .bind_stream(
                PortId::try_new("out")?,
                StreamId::try_new(format!("{id}/out"))?,
            ),
    )
    .await
    .map_err(anyhow::Error::from)
}

async fn complete_schema_probe_creation(
    core: &DrasiLib,
    handle: &ComponentHandle,
) -> anyhow::Result<()> {
    let sink = core
        .add_computation_component(
            ComponentAddition::new(
                ComponentName::try_new("schema-policy-sink")?,
                ConstructedComponent::sink(Box::new(Sink(descriptor(
                    crate::test_helpers::component_id(100),
                    false,
                )))),
            )
            .auto_start(false),
        )
        .await?;
    let output = PortId::try_new("out")?;
    let input = PortId::try_new("in")?;
    let report = apply_native_topology(core, |topology| {
        topology.relationships.push(DesiredRelationship {
            definition: EdgeDefinition {
                from: Endpoint::new(handle.id().clone(), output),
                to: Endpoint::new(sink.id().clone(), input),
            },
            policy: Default::default(),
            pipe: DesiredPipe::Bounded { capacity: 2 },
        });
    })
    .await?;
    anyhow::ensure!(
        report.applied && report.error.is_none(),
        "schema policy output binding failed: {report:?}"
    );
    handle.wait_created().await?;
    Ok(())
}

async fn preinitialized_schema_capture_remains_readable() {
    for (name, number, capability, expected) in [
        (
            "native",
            466,
            Some(Some(schema("Captured"))),
            Some(schema("Captured")),
        ),
        ("absent", 467, None, None),
        ("unsupported", 468, Some(None), None),
    ] {
        let name = ComponentName::try_new(name).unwrap();
        let has_reader = capability.is_some();
        let (native, probe, _) = source(crate::test_helpers::component_id(number), capability);
        probe.hold_initialization.store(true, Ordering::Release);
        let core = builder()
            .with_allow_external_component_ids(true)
            .build()
            .await
            .unwrap();
        let evidence: anyhow::Result<_> = async {
            let handle =
                schema_probe_bounded(add_schema_probe(&core, name.clone(), native)).await??;
            schema_probe_bounded(probe.initializing.notified()).await?;
            let captured = core.computation_control()?.registry_snapshot();
            let captured_observation = captured
                .observed
                .components
                .get(handle.id())
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("initializing source is unpublished"))?;
            let captured_has_reader = captured.source_schema_reader(handle.id())?.is_some();
            let public = core.get_source_schema_by_name(&name);
            let manager = core.source_manager();
            let managed = manager.get_source_schema_by_name(&name);
            tokio::pin!(public, managed);
            let early_public = match futures::poll!(&mut public) {
                std::task::Poll::Ready(result) => Some(result),
                std::task::Poll::Pending => None,
            };
            let early_managed = match futures::poll!(&mut managed) {
                std::task::Poll::Ready(result) => Some(result),
                std::task::Poll::Pending => None,
            };
            let public_was_early = early_public.is_some();
            let manager_was_early = early_managed.is_some();
            let calls_before_release = probe.calls.load(Ordering::Acquire);
            let public_result = match early_public {
                Some(result) => result,
                None => schema_probe_bounded(&mut public).await?,
            };
            let managed_result = match early_managed {
                Some(result) => result,
                None => schema_probe_bounded(&mut managed).await?,
            };
            let current = handle.observed();
            let current_generation = handle.generation();
            let current_has_reader = core
                .computation_control()?
                .registry_snapshot()
                .source_schema_reader(handle.id())?
                .is_some();
            let captured_result = core
                .computation_runtime
                .ensure_source_schema_current(&captured, handle.id());
            let calls_after_reads = probe.calls.load(Ordering::Acquire);
            probe.release_initialization.notify_one();
            let created =
                schema_probe_bounded(complete_schema_probe_creation(&core, &handle)).await?;
            let completed = handle.observed();
            Ok((
                captured_observation,
                captured_has_reader,
                public_was_early,
                manager_was_early,
                calls_before_release,
                created,
                public_result,
                managed_result,
                (current, current_generation, current_has_reader),
                captured_result,
                calls_after_reads,
                completed,
            ))
        }
        .await;
        release_schema_probe(&probe);
        let shutdown = core.shutdown().await;
        eprintln!("{name} stable preinitialized capture: {evidence:?}");
        shutdown.unwrap();
        let (
            captured_observation,
            captured_has_reader,
            public_was_early,
            manager_was_early,
            calls_before_release,
            created,
            public_result,
            managed_result,
            (current, current_generation, current_has_reader),
            captured_result,
            calls_after_reads,
            completed,
        ) = evidence.unwrap();
        created.unwrap();
        assert_eq!(completed.unwrap().realization, RealizationState::Created);
        assert_eq!(captured_observation.realization, RealizationState::Creating);
        assert_eq!(captured_has_reader, has_reader);
        assert_eq!(current_has_reader, has_reader);
        assert!(
            public_was_early,
            "{name}: stable capture waited for initialization: {public_result:?}"
        );
        assert!(
            manager_was_early,
            "{name}: stable manager capture waited for initialization: {managed_result:?}"
        );
        assert_eq!(public_result.unwrap(), expected);
        assert_eq!(managed_result.unwrap(), expected);
        assert_eq!(
            current.unwrap().generation,
            current_generation,
            "construction must not make the caller's current explicit handle stale"
        );
        captured_result.unwrap();
        assert_eq!(calls_before_release, if has_reader { 2 } else { 0 });
        assert_eq!(calls_after_reads, calls_before_release);
    }
}

#[tokio::test]
async fn source_schema_preinitialized_capture_remains_readable_current_thread() {
    preinitialized_schema_capture_remains_readable().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_preinitialized_capture_remains_readable_multi_thread() {
    preinitialized_schema_capture_remains_readable().await;
}

async fn schema_missing_factory_remains_explicit() {
    let name = ComponentName::try_new("missing-schema-factory").unwrap();
    let id = crate::test_helpers::component_id(470);
    let addition = ComponentAddition::declaration(
        name.clone(),
        ComponentSpecification {
            descriptor: descriptor(id.clone(), true),
            role: ComponentRole::Source,
            completion: None,
            implementation: ImplementationIdentity::try_new("test/unregistered-schema-source", "1")
                .unwrap(),
            configuration_version: 1,
            configuration: Default::default(),
            dependencies: Default::default(),
        },
    )
    .auto_start(false)
    .bind_stream(
        PortId::try_new("out").unwrap(),
        StreamId::try_new("blocked/out").unwrap(),
    );
    let core = builder()
        .with_allow_external_component_ids(true)
        .build()
        .await
        .unwrap();
    let evidence: anyhow::Result<_> = async {
        let blocked = schema_probe_bounded(core.add_computation_component(addition)).await??;
        let mut registry = core.computation_control()?.subscribe_registry();
        schema_probe_bounded(registry.wait_for(|publication| {
            publication
                .observed
                .components
                .get(&id)
                .is_some_and(|node| node.realization == RealizationState::Blocked)
        }))
        .await??;
        let public = schema_probe_bounded(core.get_source_schema_by_name(&name)).await?;
        let managed =
            schema_probe_bounded(core.source_manager().get_source_schema_by_name(&name)).await?;
        Ok((public, managed, blocked.observed()))
    }
    .await;
    let shutdown = core.shutdown().await;
    eprintln!("missing factory: {evidence:?}");
    shutdown.unwrap();
    let (blocked_public, blocked_managed, blocked_observation) = evidence.unwrap();
    let blocked_observation = blocked_observation.unwrap();
    assert_eq!(blocked_observation.realization, RealizationState::Blocked);
    assert!(!blocked_observation.waiting_for.is_empty());
    assert!(blocked_observation.failure.is_none());
    assert!(
        blocked_public.is_err(),
        "missing factory is unavailable, not a constructed source with no schema"
    );
    assert!(blocked_managed.is_err());
}

async fn schema_failed_initialization_retains_cause() {
    let failed_name = ComponentName::try_new("failed-schema-initialization").unwrap();
    let (native, probe, _) = source(crate::test_helpers::component_id(471), Some(None));
    probe.fail_initialization.store(true, Ordering::Release);
    let core = builder()
        .with_allow_external_component_ids(true)
        .build()
        .await
        .unwrap();
    let evidence: anyhow::Result<_> = async {
        let failed =
            schema_probe_bounded(add_schema_probe(&core, failed_name.clone(), native)).await??;
        let mut registry = core.computation_control()?.subscribe_registry();
        schema_probe_bounded(registry.wait_for(|publication| {
            publication
                .observed
                .components
                .get(failed.id())
                .is_some_and(|node| node.failure.is_some())
        }))
        .await??;
        let public = schema_probe_bounded(core.get_source_schema_by_name(&failed_name)).await?;
        let managed = schema_probe_bounded(
            core.source_manager()
                .get_source_schema_by_name(&failed_name),
        )
        .await?;
        Ok((public, managed, failed.observed()))
    }
    .await;
    release_schema_probe(&probe);
    let shutdown = core.shutdown().await;
    eprintln!("failed initialization: {evidence:?}");
    shutdown.unwrap();
    let (failed_public, failed_managed, failed_observation) = evidence.unwrap();
    let failed_observation = failed_observation.unwrap();
    assert!(failed_observation.failure.is_some());
    assert!(matches!(
        failed_observation.failure.as_ref().unwrap().cause.underlying(),
        GraphError::Component { operation: "initialize", source, .. }
            if source.downcast_ref::<SchemaInitializationFailure>().is_some()
    ));
    assert!(
        failed_public.is_err(),
        "failed initialization must not be replaced by a successful None"
    );
    assert!(failed_managed.is_err());
    let failed_public = failed_public.unwrap_err();
    let failed_managed = failed_managed.unwrap_err();
    for error in [
        failed_public.downcast_ref::<GraphError>().unwrap(),
        failed_managed.downcast_ref::<GraphError>().unwrap(),
    ] {
        assert!(matches!(
            error.underlying(),
            GraphError::Component { operation: "initialize", source, .. }
                if source.downcast_ref::<SchemaInitializationFailure>().is_some()
        ));
    }
    assert_eq!(probe.calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn source_schema_missing_factory_remains_explicit_current_thread() {
    schema_missing_factory_remains_explicit().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_missing_factory_remains_explicit_multi_thread() {
    schema_missing_factory_remains_explicit().await;
}

#[tokio::test]
async fn source_schema_failed_initialization_retains_cause_current_thread() {
    schema_failed_initialization_retains_cause().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_failed_initialization_retains_cause_multi_thread() {
    schema_failed_initialization_retains_cause().await;
}

struct DeferredSchemaFactory {
    descriptor: FactoryDescriptor,
    probe: Arc<Probe>,
    reader: Option<Arc<dyn SourceSchemaReader>>,
}

#[async_trait]
impl ComponentFactory for DeferredSchemaFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        self.probe.constructing.notify_one();
        self.probe.release_construction.notified().await;
        Ok(ConstructedComponent::source(Box::new(NativeSource {
            descriptor: context.specification.descriptor.clone(),
            reader: self.reader.clone(),
            probe: self.probe.clone(),
        })))
    }
}

async fn fresh_schema_reads_join_real_deferred_construction() {
    let mut premature_reads = Vec::new();
    for (name, number, capability, expected) in [
        (
            "native",
            466,
            Some(Some(schema("Constructed"))),
            Some(schema("Constructed")),
        ),
        ("absent", 467, None, None),
        ("unsupported", 468, Some(None), None),
    ] {
        let name = ComponentName::try_new(name).unwrap();
        let has_reader = capability.is_some();
        let id = crate::test_helpers::component_id(number);
        let (native, probe, _) = source(id.clone(), capability);
        probe.hold_initialization.store(true, Ordering::Release);
        let factory = Arc::new(DeferredSchemaFactory {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("test/deferred-schema-source", "1")
                    .unwrap(),
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: Default::default(),
                dependencies: Default::default(),
            },
            probe: probe.clone(),
            reader: native.reader,
        });
        let addition = ComponentAddition::from_specification(
            name.clone(),
            ComponentSpecification {
                descriptor: descriptor(id.clone(), true),
                role: ComponentRole::Source,
                completion: None,
                implementation: factory.descriptor.implementation.clone(),
                configuration_version: 1,
                configuration: Default::default(),
                dependencies: Default::default(),
            },
            factory,
        )
        .auto_start(false)
        .bind_stream(
            PortId::try_new("out").unwrap(),
            StreamId::try_new(format!("{id}/out")).unwrap(),
        );
        let core = builder()
            .with_allow_external_component_ids(true)
            .build()
            .await
            .unwrap();
        let evidence: anyhow::Result<_> = async {
            let handle = schema_probe_bounded(core.add_computation_component(addition)).await??;
            schema_probe_bounded(probe.constructing.notified()).await?;
            let control = core.computation_control()?;
            let captured = control.registry_snapshot();
            let captured_observation = captured
                .observed
                .components
                .get(handle.id())
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("constructing source is unpublished"))?;
            let captured_has_reader = captured.source_schema_reader(handle.id())?.is_some();
            let public = core.get_source_schema_by_name(&name);
            let manager = core.source_manager();
            let managed = manager.get_source_schema_by_name(&name);
            let aggregate = core.get_graph_schema();
            tokio::pin!(public, managed, aggregate);
            let early_public = match futures::poll!(&mut public) {
                std::task::Poll::Ready(result) => Some(result),
                std::task::Poll::Pending => None,
            };
            let early_managed = match futures::poll!(&mut managed) {
                std::task::Poll::Ready(result) => Some(result),
                std::task::Poll::Pending => None,
            };
            let early_aggregate = match futures::poll!(&mut aggregate) {
                std::task::Poll::Ready(result) => Some(result),
                std::task::Poll::Pending => None,
            };
            let early = (
                early_public.is_some(),
                early_managed.is_some(),
                early_aggregate.is_some(),
            );
            let calls_before_release = probe.calls.load(Ordering::Acquire);
            probe.release_construction.notify_one();
            schema_probe_bounded(probe.initializing.notified()).await?;
            let public_result = match early_public {
                Some(result) => result,
                None => schema_probe_bounded(&mut public).await?,
            };
            let managed_result = match early_managed {
                Some(result) => result,
                None => schema_probe_bounded(&mut managed).await?,
            };
            let aggregate_result = match early_aggregate {
                Some(result) => result,
                None => schema_probe_bounded(&mut aggregate).await?,
            };
            let readable_while_initializing = handle.observed();
            let current_has_reader = control
                .registry_snapshot()
                .source_schema_reader(handle.id())?
                .is_some();
            let captured_result = core
                .computation_runtime
                .ensure_source_schema_current(&captured, handle.id());
            let calls_after_reads = probe.calls.load(Ordering::Acquire);
            probe.release_initialization.notify_one();
            let created =
                schema_probe_bounded(complete_schema_probe_creation(&core, &handle)).await?;
            let completed = handle.observed();
            Ok((
                captured_observation,
                captured_has_reader,
                early,
                calls_before_release,
                public_result,
                managed_result,
                aggregate_result,
                (readable_while_initializing, current_has_reader),
                captured_result,
                calls_after_reads,
                created,
                completed,
            ))
        }
        .await;
        release_schema_probe(&probe);
        let shutdown = core.shutdown().await;
        eprintln!("{name} deferred construction and capture-before-initialize: {evidence:?}");
        shutdown.unwrap();
        let (
            captured_observation,
            captured_has_reader,
            early,
            calls_before_release,
            public_result,
            managed_result,
            aggregate_result,
            (readable_while_initializing, current_has_reader),
            captured_result,
            calls_after_reads,
            created,
            completed,
        ) = evidence.unwrap();
        created.unwrap();
        assert_eq!(completed.unwrap().realization, RealizationState::Created);
        assert_eq!(captured_observation.realization, RealizationState::Creating);
        assert!(!captured_has_reader);
        assert_eq!(current_has_reader, has_reader);
        assert_eq!(calls_before_release, 0);
        let current = readable_while_initializing.unwrap();
        assert_eq!(current.realization, RealizationState::Creating);
        assert_eq!(current.generation, captured_observation.generation);
        assert_eq!(current.operation, captured_observation.operation);
        if has_reader {
            assert!(matches!(
                captured_result.unwrap_err().downcast_ref::<GraphError>(),
                Some(GraphError::StaleGeneration)
            ));
        } else {
            captured_result.unwrap();
        }
        if early != (false, false, false) {
            premature_reads.push(format!(
                "{name}: public/manager/aggregate early={early:?}; public={public_result:?}; manager={managed_result:?}; aggregate={aggregate_result:?}"
            ));
            continue;
        }
        assert_eq!(public_result.unwrap(), expected);
        assert_eq!(managed_result.unwrap(), expected);
        let aggregate = aggregate_result.unwrap();
        if expected.is_some() {
            assert!(aggregate.nodes["Constructed"].sources.contains(&id));
            assert!(!aggregate.sources_without_schema.contains(&id));
        } else {
            assert!(aggregate.nodes.is_empty());
            assert!(aggregate.sources_without_schema.contains(&id));
        }
        assert_eq!(calls_after_reads, if has_reader { 3 } else { 0 });
    }
    assert!(
        premature_reads.is_empty(),
        "fresh reads used unresolved factory captures:\n{}",
        premature_reads.join("\n")
    );
}

#[tokio::test]
async fn source_schema_real_deferred_construction_capture_current_thread() {
    fresh_schema_reads_join_real_deferred_construction().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_real_deferred_construction_capture_multi_thread() {
    fresh_schema_reads_join_real_deferred_construction().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_discovery_multi_thread() {
    discovery().await;
}

async fn fresh_schema_initial_creation_capture(manager_path: bool) {
    use super::super::inspection::{SourceSchemaCaptureObservation, SourceSchemaCaptureTest};

    struct Evidence {
        declared: ObservedComponent,
        creating: ObservedComponent,
        early_capture: Option<SourceSchemaCaptureObservation>,
        early_result: bool,
        result: anyhow::Result<Option<SourceSchema>>,
    }

    let core = builder()
        .with_allow_external_component_ids(true)
        .build()
        .await
        .unwrap();
    let id = crate::test_helpers::component_id(467);
    let name = ComponentName::try_new("absent").unwrap();
    let (native, probe, _) = source(id.clone(), None);
    probe.hold_initialization.store(true, Ordering::Release);
    let committed = Arc::new(Notify::new());
    let release_creation = Arc::new(Notify::new());
    let release_capture = Arc::new(Notify::new());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    *core
        .computation_runtime
        .source_schema_capture_test
        .lock()
        .expect("source schema test hook lock") = Some(Arc::new(SourceSchemaCaptureTest {
        component: id.clone(),
        release: release_capture.clone(),
        observations: observations.clone(),
    }));
    let mut admitted = None;
    let evidence: anyhow::Result<Evidence> = async {
        let control = core.computation_control()?;
        tokio::time::timeout(
            Duration::from_secs(3),
            control.gate_source_schema_initial_creation_for_test(
                id.clone(),
                committed.clone(),
                release_creation.clone(),
            ),
        )
        .await??;
        let handle = tokio::time::timeout(
            Duration::from_secs(3),
            core.add_computation_component(
                ComponentAddition::new(
                    name.clone(),
                    ConstructedComponent::source(Box::new(native)),
                )
                .auto_start(false)
                .bind_stream(
                    PortId::try_new("out")?,
                    StreamId::try_new(format!("{id}/out"))?,
                ),
            ),
        )
        .await??;
        admitted = Some(handle.clone());
        tokio::time::timeout(Duration::from_secs(3), committed.notified()).await?;
        let publication = control.registry_snapshot();
        let declared = publication
            .observed
            .components
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("committed source has no observation"))?;
        anyhow::ensure!(
            publication.source_schema_reader(&id)?.is_none(),
            "absent source unexpectedly published a reader"
        );
        let manager = core.source_manager();
        let read = async {
            if manager_path {
                manager.get_source_schema_by_name(&name).await
            } else {
                core.get_source_schema_by_name(&name)
                    .await
                    .map_err(anyhow::Error::from)
            }
        };
        tokio::pin!(read);
        let early = match futures::poll!(&mut read) {
            std::task::Poll::Ready(result) => Some(result),
            std::task::Poll::Pending => None,
        };
        let early_result = early.is_some();
        let early_capture = observations
            .lock()
            .expect("source schema test observation lock")
            .iter()
            .find(|observation| observation.stage == "fresh-capture")
            .cloned();
        release_creation.notify_one();
        tokio::time::timeout(Duration::from_secs(3), probe.initializing.notified()).await?;
        let creating = control
            .registry_snapshot()
            .observed
            .components
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("creating source has no observation"))?;
        release_capture.notify_one();
        let result = match early {
            Some(result) => result,
            None => tokio::time::timeout(Duration::from_secs(3), &mut read).await?,
        };
        Ok(Evidence {
            declared,
            creating,
            early_capture,
            early_result,
            result,
        })
    }
    .await;

    release_creation.notify_one();
    release_capture.notify_one();
    probe.release_initialization.notify_one();
    let before_binding = admitted.as_ref().map(|handle| handle.observed());
    let created = match admitted.as_ref() {
        Some(handle) => Some(
            tokio::time::timeout(Duration::from_secs(3), async {
                // A stream declaration alone does not bind the source's output.
                // Connect only after the same-invocation capture evidence is complete.
                let sink = core
                    .add_computation_component(
                        ComponentAddition::new(
                            ComponentName::try_new("schema-seam-sink")?,
                            ConstructedComponent::sink(Box::new(Sink(descriptor(
                                crate::test_helpers::component_id(100),
                                false,
                            )))),
                        )
                        .auto_start(false),
                    )
                    .await?;
                let output = PortId::try_new("out")?;
                let input = PortId::try_new("in")?;
                let report = apply_native_topology(&core, |topology| {
                    topology.relationships.push(DesiredRelationship {
                        definition: EdgeDefinition {
                            from: Endpoint::new(handle.id().clone(), output),
                            to: Endpoint::new(sink.id().clone(), input),
                        },
                        policy: Default::default(),
                        pipe: DesiredPipe::Bounded { capacity: 2 },
                    });
                })
                .await?;
                anyhow::ensure!(
                    report.applied && report.error.is_none(),
                    "schema seam output binding failed: {report:?}"
                );
                handle.wait_created().await?;
                Ok::<(), anyhow::Error>(())
            })
            .await,
        ),
        None => None,
    };
    let after_binding = admitted.as_ref().map(|handle| handle.observed());
    let shutdown = core.shutdown().await;
    let observations = observations
        .lock()
        .expect("source schema test observation lock")
        .clone();
    eprintln!(
        "fresh absent467 {} initial-creation capture: observations={observations:#?}",
        if manager_path { "manager" } else { "public" }
    );
    match &evidence {
        Ok(evidence) => eprintln!(
            "declared={:?}; creating={:?}; early_capture={:?}; early_result={}; result={:?}",
            evidence.declared,
            evidence.creating,
            evidence.early_capture,
            evidence.early_result,
            evidence.result,
        ),
        Err(error) => eprintln!("source schema seam setup/read failed: {error:#}"),
    }
    eprintln!(
        "source schema seam completion AFTER gate releases and awaited library shutdown: \
         before_binding={before_binding:#?}; after_binding={after_binding:#?}; \
         created={created:#?}; shutdown={shutdown:#?}"
    );
    shutdown.unwrap();
    let evidence = evidence.unwrap();
    created.unwrap().unwrap().unwrap();
    assert_eq!(
        after_binding.unwrap().unwrap().realization,
        RealizationState::Created,
        "the connected source must finish actual construction before shutdown"
    );
    assert_eq!(evidence.declared.realization, RealizationState::Pending);
    assert_eq!(evidence.creating.realization, RealizationState::Creating);
    assert_eq!(evidence.declared.generation, evidence.creating.generation);
    assert_ne!(evidence.declared.operation, evidence.creating.operation);
    assert!(!evidence.early_result);
    assert_eq!(evidence.result.unwrap(), None);
    assert!(
        evidence.early_capture.is_none(),
        "fresh internal capture was established before initial construction reserved its operation"
    );
    let capture = observations
        .iter()
        .find(|observation| observation.stage == "fresh-capture")
        .expect("the real fresh API must establish its own capture");
    let captured = capture.captured.as_ref().unwrap();
    assert_eq!(captured.generation, evidence.creating.generation);
    assert_eq!(captured.operation, evidence.creating.operation);
    assert_eq!(captured.realization, RealizationState::Creating);
    assert_eq!(capture.captured_reader.as_ref().unwrap(), &false);
    assert_eq!(capture.current_reader.as_ref().unwrap(), &false);
    let current = capture.current.as_ref().unwrap();
    assert_eq!(current.generation, captured.generation);
    assert_eq!(current.operation, captured.operation);
    assert!(observations
        .iter()
        .any(|observation| observation.stage == "ordinary-fallback-completed"));
    assert!(!observations
        .iter()
        .any(|observation| observation.stage == "currency-rejected"));
    assert_eq!(probe.calls.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn source_schema_fresh_public_initial_creation_fence_current_thread() {
    fresh_schema_initial_creation_capture(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_fresh_public_initial_creation_fence_multi_thread() {
    fresh_schema_initial_creation_capture(false).await;
}

#[tokio::test]
async fn source_schema_fresh_manager_initial_creation_fence_current_thread() {
    fresh_schema_initial_creation_capture(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_schema_fresh_manager_initial_creation_fence_multi_thread() {
    fresh_schema_initial_creation_capture(true).await;
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
