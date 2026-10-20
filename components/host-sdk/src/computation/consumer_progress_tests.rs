// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{consumer_progress_fixture as fixture, *};
use drasi_core::{computation::InMemoryComputationProvider, interface::StorageDurability};
use futures_util::poll;
use sdk::Factory as _;
use std::sync::{atomic::AtomicBool, OnceLock};

const GRAPH: &str = "native-consumer-progress";
fn id(name: &str) -> ComponentId {
    ComponentId::try_new(name).unwrap()
}
fn scope() -> sdk::Scope {
    sdk::Scope {
        instance_id: GRAPH.into(),
        graph_id: GRAPH.into(),
        generation: 1,
    }
}
fn definition() -> anyhow::Result<sdk::PluginDefinition> {
    sdk::PluginDefinition::new(
        "fixture/consumer-progress",
        "1",
        fixture::factories(),
        vec![GraphChangeCodec::schema()],
    )
}
fn plugin() -> anyhow::Result<Arc<NativePlugin>> {
    NativePlugin::from_definition(definition()?)
}
fn factory(plugin: &NativePlugin, role: ComponentRole) -> Arc<NativeFactory> {
    plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().role == role
                && factory
                    .metadata()
                    .implementation
                    .name
                    .starts_with("fixture/consumer-progress-")
        })
        .unwrap()
        .clone()
}
fn sink_factory(plugin: &NativePlugin, completion: SinkCompletion) -> Arc<NativeFactory> {
    plugin
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().role == ComponentRole::Sink
                && factory.metadata().completion == Some(completion)
                && factory
                    .metadata()
                    .implementation
                    .name
                    .starts_with("fixture/input-progress-")
        })
        .expect("native sink fixture")
        .clone()
}
fn owners() -> Arc<ConsumerProgressSet> {
    Arc::new(
        ConsumerProgressSet::new(
            GRAPH,
            id("producer"),
            ["first", "second"]
                .map(|name| Arc::new(QuerySourceProgress::new(GRAPH, id(name)).unwrap())),
        )
        .unwrap(),
    )
}
fn create(
    factory: &NativeFactory,
    mode: &str,
    progress: Arc<ConsumerProgressSet>,
) -> anyhow::Result<NativeComponentProxy> {
    factory.create_component_with_consumer_progress(
        id("producer"),
        json!({"mode": mode}),
        scope(),
        progress,
    )
}
fn command(kind: &str, payload: Value) -> anyhow::Result<Vec<u8>> {
    sdk::wire::encode(&sdk::ControlMessage {
        from: id("first"),
        generation: 1,
        direction: sdk::ControlDirection::Upstream,
        notification: sdk::ControlNotification::Custom {
            kind: kind.into(),
            payload,
        },
    })
}
async fn control(proxy: &NativeComponentProxy, kind: &str, payload: Value) -> anyhow::Result<()> {
    proxy
        .inner
        .unit(abi::operation::CONTROL, &command(kind, payload)?)
        .await
}
fn closed(error: &anyhow::Error) {
    assert_eq!(
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<sdk::transport::Failure>())
            .unwrap()
            .code,
        abi::status::CLOSED
    );
}
async fn query(owner: Arc<QuerySourceProgress>) -> anyhow::Result<ContinuousQueryTransformer> {
    ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: GRAPH.into(),
            id: owner.component_id().clone(),
            query: "MATCH (n:Item) RETURN n.value AS value".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new(format!("{}/out", owner.component_id()))?,
            outbox_capacity: NonZeroUsize::new(8).unwrap(),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await?
    .with_source_progress(owner)
}

#[tokio::test(flavor = "current_thread")]
async fn consumer_progress_static_and_dynamic_producers_observe_actual_independent_owners(
) -> anyhow::Result<()> {
    for plugin in [
        plugin()?,
        super::super::factory::recovery_tests::sqlite_plugin()?,
    ] {
        for role in [ComponentRole::Source, ComponentRole::Transformer] {
            let factory = factory(&plugin, role);
            let progress = owners();
            let mut first = query(progress.owners()[&id("first")].clone()).await?;
            let mut second = query(progress.owners()[&id("second")].clone()).await?;
            let retained = role == ComponentRole::Source;
            let mut proxy = create(
                &factory,
                if retained { "retained" } else { "valid" },
                progress.clone(),
            )?;
            let recovery = if retained {
                ComponentRecovery::admitted(StorageDurability::LOCAL_PROCESS_RESTART)
            } else {
                ComponentRecovery::default()
            };
            assert_eq!(
                proxy.recovery_contract(),
                recovery.clone().with_consumer_progress(progress.clone())
            );
            let lookalike = Arc::new(ConsumerProgressSet::new(
                GRAPH,
                id("producer"),
                progress.owners().values().cloned(),
            )?);
            assert_ne!(
                proxy.recovery_contract(),
                recovery.with_consumer_progress(lookalike)
            );
            assert!(proxy.recovery_progress().is_none());
            closed(&control(&proxy, "snapshot", Value::Null).await.unwrap_err());
            proxy.start().await?;
            control(
                &proxy,
                "expect",
                fixture::snapshot_json(&progress.read_only().snapshot()?)?,
            )
            .await?;
            first.start().await?;
            let partial = progress.read_only().snapshot()?;
            assert!(partial.consumers[&id("first")].ready);
            assert!(!partial.consumers[&id("second")].ready && !partial.ready());
            control(&proxy, "expect", fixture::snapshot_json(&partial)?).await?;
            second.start().await?;
            let input = super::envelope(&id("producer"), 1);
            let outputs = first
                .transform(InputEnvelope {
                    port: PortId::try_new("in")?,
                    envelope: input.clone(),
                })
                .await?;
            first.delivery_completed(&outputs).await?;
            let snapshot = progress.read_only().snapshot()?;
            assert!(snapshot.recovered());
            assert!(!snapshot.consumers[&id("first")].checkpoints.is_empty());
            assert!(snapshot.consumers[&id("second")].checkpoints.is_empty());
            assert!(snapshot.consumers.values().all(|state| !state.persistent));
            control(&proxy, "expect", fixture::snapshot_json(&snapshot)?).await?;
            if role == ComponentRole::Source {
                assert!(proxy.next().await?.is_none());
            } else {
                let outputs = proxy
                    .transform(InputEnvelope {
                        port: PortId::try_new("in")?,
                        envelope: input,
                    })
                    .await?;
                assert_eq!(outputs.len(), 1);
                proxy.delivery_completed(&outputs).await?;
            }
            control(&proxy, "subscribe", Value::Null).await?;
            let remote = proxy.inner.clone();
            let bytes = command("wait", Value::Null)?;
            let mut pending = Box::pin(remote.unit(abi::operation::CONTROL, &bytes));
            assert!(poll!(pending.as_mut()).is_pending());
            proxy.stop().await?;
            closed(
                &tokio::time::timeout(Duration::from_secs(1), pending)
                    .await?
                    .unwrap_err(),
            );
            proxy.start().await?;
            closed(
                &control(&proxy, "old-snapshot", Value::Null)
                    .await
                    .unwrap_err(),
            );
            control(&proxy, "snapshot", Value::Null).await?;
            control(&proxy, "subscribe", Value::Null).await?;
            let mut pending = Box::pin(remote.unit(abi::operation::CONTROL, &bytes));
            assert!(poll!(pending.as_mut()).is_pending());
            drop(proxy);
            closed(
                &tokio::time::timeout(Duration::from_secs(1), pending)
                    .await?
                    .unwrap_err(),
            );
            closed(
                &remote
                    .unit(abi::operation::CONTROL, &command("snapshot", Value::Null)?)
                    .await
                    .unwrap_err(),
            );
            first.stop().await?;
            second.stop().await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn consumer_progress_rejects_missing_substituted_conflicting_and_mutating_readers(
) -> anyhow::Result<()> {
    let plugin = plugin()?;
    for role in [ComponentRole::Source, ComponentRole::Transformer] {
        let factory = factory(&plugin, role);
        let progress = owners();
        assert!(factory
            .create_component(id("producer"), json!({"mode":"valid"}))
            .is_err());
        for mode in ["ignored", "substituted", "unstable-policy"] {
            assert!(create(&factory, mode, progress.clone()).is_err());
        }
        if role == ComponentRole::Source {
            assert!(create(&factory, "dual", progress.clone()).is_err());
        }
        for mode in ["mutate-reader", "mutate-retention", "mutate-policy"] {
            let mut proxy = create(&factory, mode, progress.clone())?;
            assert!(proxy.start().await.is_err());
            closed(&control(&proxy, "snapshot", Value::Null).await.unwrap_err());
            assert!(proxy.start().await.is_err());
            proxy.stop().await?;
        }
        assert!(factory
            .create_component_with_consumer_progress(
                id("wrong-producer"),
                json!({"mode":"valid"}),
                scope(),
                progress.clone(),
            )
            .is_err());
        let mut wrong = scope();
        wrong.graph_id = "other".into();
        assert!(factory
            .create_component_with_consumer_progress(
                id("producer"),
                json!({"mode":"valid"}),
                wrong,
                progress,
            )
            .is_err());
    }
    for (role, legacy) in [
        (ComponentRole::Sink, false),
        (ComponentRole::Service, false),
        (ComponentRole::Source, true),
    ] {
        let definition = sdk::PluginDefinition::new(
            "fixture/invalid",
            "1",
            vec![Arc::new(fixture::Factory(role, legacy))],
            vec![GraphChangeCodec::schema()],
        )?;
        assert!(NativePlugin::from_definition(definition).is_err());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn consumer_progress_failed_and_cancelled_lifecycle_requires_successful_cleanup(
) -> anyhow::Result<()> {
    let factory = factory(plugin()?.as_ref(), ComponentRole::Source);
    for mode in [
        "start-fail-once",
        "start-pending-once",
        "stop-fail-once",
        "stop-pending-once",
    ] {
        let mut proxy = create(&factory, mode, owners())?;
        if mode == "start-pending-once" {
            let mut start = Box::pin(proxy.start());
            assert!(poll!(start.as_mut()).is_pending());
            drop(start);
        } else if mode == "start-fail-once" {
            assert!(proxy.start().await.is_err());
        } else {
            proxy.start().await?;
            if mode == "stop-pending-once" {
                let mut stop = Box::pin(proxy.stop());
                assert!(poll!(stop.as_mut()).is_pending());
                drop(stop);
            } else {
                assert!(proxy.stop().await.is_err());
            }
        }
        closed(&control(&proxy, "snapshot", Value::Null).await.unwrap_err());
        assert!(
            proxy.start().await.is_err(),
            "{mode} must not hide incomplete cleanup"
        );
        proxy.stop().await?;
        proxy.start().await?;
        control(&proxy, "snapshot", Value::Null).await?;
        proxy.stop().await?;
    }
    let mut proxy = create(&factory, "wait-recovered", owners())?;
    let mut start = Box::pin(proxy.start());
    assert!(poll!(start.as_mut()).is_pending());
    drop(start);
    closed(&control(&proxy, "snapshot", Value::Null).await.unwrap_err());
    proxy.stop().await?;
    Ok(())
}

#[test]
fn consumer_progress_required_resource_shape_is_explicit() -> anyhow::Result<()> {
    let factory = factory(plugin()?.as_ref(), ComponentRole::Source);
    let requirement = &factory.descriptor().dependencies["consumer_progress"];
    assert_eq!(requirement.minimum, 1);
    assert_eq!(requirement.maximum, Some(1));
    assert_eq!(requirement.role, ResourceRole::Checkpoint);
    assert_eq!(
        requirement.concrete_type,
        Some(std::any::TypeId::of::<ConsumerProgressSet>())
    );
    let mut spec = factory.specification(id("producer"), json!({"mode":"valid"}))?;
    assert!(factory.validate(&spec).is_err());
    let resource = ResourceId::try_new("progress")?;
    for bindings in [vec![], vec![resource.clone(), resource.clone()]] {
        spec.dependencies
            .insert("consumer_progress".into(), bindings);
        assert!(factory.validate(&spec).is_err());
    }

    spec.dependencies
        .insert("consumer_progress".into(), vec![resource.clone()]);
    factory.validate(&spec)?;
    spec.dependencies
        .insert("source_progress".into(), vec![resource.clone()]);
    assert!(factory.validate(&spec).is_err());
    spec.dependencies.remove("source_progress");
    spec.dependencies.insert("admission".into(), vec![resource]);
    assert!(factory.validate(&spec).is_err());
    Ok(())
}

#[test]
fn consumer_progress_input_policy_cannot_be_exported_without_its_required_binding(
) -> anyhow::Result<()> {
    struct Compare;
    impl InputPositionComparator for Compare {
        fn is_after(&self, _: &[u8], _: &[u8]) -> anyhow::Result<bool> {
            Ok(true)
        }
    }
    struct Source {
        descriptor: ComponentDescriptor,
        policy: Arc<InputPositionPolicy>,
    }
    #[async_trait]
    impl sdk::NativeComponent for Source {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.descriptor
        }
        fn configuration(&self) -> anyhow::Result<Value> {
            Ok(json!({"mode":"policy-only"}))
        }
        fn input_position_policy(&self) -> Option<Arc<InputPositionPolicy>> {
            Some(self.policy.clone())
        }
        async fn start(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }
    #[async_trait]
    impl sdk::NativeSource for Source {
        async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
            Ok(None)
        }
    }
    struct Factory;
    impl sdk::Factory for Factory {
        fn metadata(&self) -> sdk::FactoryMetadata {
            let mut metadata = fixture::Factory(ComponentRole::Source, false).metadata();
            metadata.capabilities.control = false;
            metadata
        }
        fn create(
            &self,
            request: &sdk::CreateRequest,
            _: sdk::ControlSender,
        ) -> anyhow::Result<sdk::CreatedComponent> {
            Ok(sdk::Component::Source(Box::new(Source {
                descriptor: self.metadata().descriptor(request.id.clone())?,
                policy: Arc::new(InputPositionPolicy::new(
                    SourceProgressKey::Source("source".into()),
                    Arc::new(Compare),
                )?),
            }))
            .into())
        }
    }
    let plugin = NativePlugin::from_definition(sdk::PluginDefinition::new(
        "fixture/unbound-position",
        "1",
        vec![Arc::new(Factory)],
        vec![GraphChangeCodec::schema()],
    )?)?;
    let error = plugin.factories()[0]
        .create_component(id("producer"), json!({"mode":"policy-only"}))
        .err()
        .expect("unbound policy must fail");
    assert!(error
        .to_string()
        .contains("requires its negotiated service"));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn consumer_progress_graph_factory_resolves_the_actual_typed_checkpoint_resource(
) -> anyhow::Result<()> {
    struct Drain(ComponentDescriptor);
    #[async_trait]
    impl ComputationComponent for Drain {
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
    impl EnvelopeSink for Drain {
        fn completion(&self) -> SinkCompletion {
            SinkCompletion::Handled
        }
        async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
            Ok(())
        }
    }
    let factory = factory(plugin()?.as_ref(), ComponentRole::Source);
    for wrong_type in [false, true] {
        let owner = Arc::new(QuerySourceProgress::new(GRAPH, id("first"))?);
        let progress = Arc::new(ConsumerProgressSet::new(
            GRAPH,
            id("producer"),
            [owner.clone()],
        )?);
        let resource = ResourceId::try_new("progress")?;
        let mut specification = factory.specification(id("producer"), json!({"mode":"valid"}))?;
        specification
            .dependencies
            .insert("consumer_progress".into(), vec![resource.clone()]);
        let handle = if wrong_type {
            ResourceHandle::new(ResourceRole::Checkpoint, owner.clone())
        } else {
            ResourceHandle::new(ResourceRole::Checkpoint, progress)
        };
        let endpoint = |component: &str, port: &str| {
            Endpoint::new(id(component), PortId::try_new(port).unwrap())
        };
        let graph = ComputationGraph::builder(GRAPH)
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::Checkpoint,
                ownership: ResourceOwnership::Borrowed,
                binding: "progress".into(),
            })?
            .provide_resource(resource, handle)?
            .component(specification, factory.clone())
            .query(Box::new(query(owner).await?))
            .sink(Box::new(Drain(ComponentDescriptor::try_new(
                id("drain"),
                vec![PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?)))
            .bind_stream(
                endpoint("producer", "out"),
                StreamId::try_new("producer/out")?,
            )
            .bind_stream(endpoint("first", "out"), StreamId::try_new("first/out")?)
            .connect(
                EdgeDefinition::new(endpoint("producer", "out"), endpoint("first", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("first", "out"), endpoint("drain", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build();
        if wrong_type {
            assert!(graph.is_err());
        } else {
            let mut graph = graph?;
            let run = graph.start()?;
            let control = run.control();
            let (result, startup) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(run, async {
                    let startup = control.startup_report().await;
                    control.cancel();
                    startup
                })
            })
            .await?;
            assert_eq!(startup?.summary, OperationSummary::Completed);
            assert!(matches!(result, Ok(()) | Err(GraphError::Cancelled)));
            graph.shutdown().await?;
        }
    }
    Ok(())
}
fn export() -> &'static sdk::ExportedPlugin {
    static EXPORT: OnceLock<sdk::ExportedPlugin> = OnceLock::new();
    EXPORT.get_or_init(|| sdk::ExportedPlugin::new(definition().unwrap()).unwrap())
}
unsafe extern "C" fn metadata() -> *const abi::Metadata {
    export().metadata()
}

#[test]
fn consumer_progress_malformed_extension_rejects_before_entry() {
    static ENTERED: AtomicBool = AtomicBool::new(false);
    unsafe extern "C" fn entry(_: *mut abi::PluginHandle) -> abi::Status {
        ENTERED.store(true, Ordering::Release);
        abi::Status::ok()
    }
    unsafe extern "C" fn extension() -> *const abi::consumer_progress::PluginConsumerProgressV1 {
        static TABLE: abi::consumer_progress::PluginConsumerProgressV1 =
            abi::consumer_progress::PluginConsumerProgressV1 {
                header: abi::Header::new::<abi::consumer_progress::PluginConsumerProgressV1>(),
                version: abi::consumer_progress::VERSION + 1,
                reserved: 0,
                factory: None,
                create: None,
                inspect: None,
                position_policy: None,
            };
        &TABLE
    }
    assert!(unsafe {
        NativePlugin::from_entry_points_with_consumer_progress(
            metadata,
            entry,
            None,
            None,
            Some(extension),
        )
    }
    .is_err());
    assert!(!ENTERED.load(Ordering::Acquire));
}

#[test]
fn consumer_progress_sdk_rejects_unbound_base_creation_even_without_host_negotiation(
) -> anyhow::Result<()> {
    let export = export();
    let mut plugin = abi::PluginHandle::null();
    let mut factory = abi::FactoryHandle::null();
    let mut component = abi::ComponentHandle::null();
    unsafe {
        export.entry(&mut plugin)?;
        sdk::transport::take_status((*plugin.vtable).factory.unwrap()(
            plugin.state,
            0,
            &mut factory,
        ))?;
        let request = sdk::CreateRequest {
            id: id("producer"),
            implementation: fixture::Factory(ComponentRole::Source, false)
                .metadata()
                .implementation,
            configuration_version: 1,
            configuration: json!({"mode":"valid"}),
            scope: Some(scope()),
        };
        // The SDK supplies provenance in its catalogue, so use the negotiated identity.
        let metadata: sdk::PluginMetadata = serde_json::from_slice(
            sdk::transport::borrowed_bytes((*export.metadata()).manifest, abi::MAX_METADATA_BYTES)?,
        )?;
        let request = sdk::CreateRequest {
            implementation: metadata.factories[0].implementation.clone(),
            ..request
        };
        let bytes = sdk::wire::encode(&request)?;
        let result = sdk::transport::take_status((*factory.vtable).create.unwrap()(
            factory.state,
            abi::BorrowedBytes::new(&bytes),
            &mut component,
        ));
        assert!(result.is_err());
        assert!(component.state.is_null());
        (*factory.vtable).release.unwrap()(factory.state);
        (*plugin.vtable).release.unwrap()(plugin.state);
    }
    Ok(())
}

#[test]
fn native_input_progress_binding_is_optional_scoped_and_not_atomic() -> anyhow::Result<()> {
    for plugin in [
        plugin()?,
        super::super::factory::recovery_tests::sqlite_plugin()?,
    ] {
        let handled = sink_factory(&plugin, SinkCompletion::Handled);
        let requirement = &handled.descriptor().dependencies["input_progress"];
        assert_eq!(requirement.role, ResourceRole::Checkpoint);
        assert_eq!((requirement.minimum, requirement.maximum), (0, Some(1)));
        assert_eq!(
            requirement.concrete_type,
            Some(std::any::TypeId::of::<QuerySourceProgressResource>())
        );
        let untracked = handled.create_component(id("sink"), json!({"mode":"fanout"}))?;
        assert_eq!(untracked.recovery_contract(), ComponentRecovery::default());
        let owner = Arc::new(QuerySourceProgress::new(GRAPH, id("sink"))?);
        let tracked = handled.create_component_with_input_progress(
            id("sink"),
            json!({"mode":"fanout"}),
            scope(),
            owner.clone(),
            None,
        )?;
        assert_eq!(
            tracked.recovery_contract(),
            ComponentRecovery::default().with_volatile_input_progress(owner.clone())
        );
        assert!(owner.snapshot().checkpoints.is_empty() && !owner.snapshot().persistent);
        for wrong in [
            Arc::new(QuerySourceProgress::new("wrong", id("sink"))?),
            Arc::new(QuerySourceProgress::new(GRAPH, id("wrong"))?),
        ] {
            assert!(handled
                .create_component_with_input_progress(
                    id("sink"),
                    json!({"mode":"fanout"}),
                    scope(),
                    wrong,
                    None,
                )
                .is_err());
        }
        let mut specification = handled.specification(id("sink"), json!({"mode":"fanout"}))?;
        handled.validate(&specification)?;
        specification.dependencies.insert(
            "input_progress".into(),
            vec![ResourceId::try_new("progress")?],
        );
        handled.validate(&specification)?;
        specification
            .dependencies
            .get_mut("input_progress")
            .unwrap()
            .push(ResourceId::try_new("second")?);
        assert!(handled.validate(&specification).is_err());
        for invalid in plugin.factories().iter().filter(|factory| {
            factory.metadata().role == ComponentRole::Source
                || factory.metadata().completion == Some(SinkCompletion::Accepted)
                || factory.consumer_mode().is_some()
        }) {
            assert!(!invalid
                .descriptor()
                .dependencies
                .contains_key("input_progress"));
            assert!(invalid
                .create_component_with_input_progress(
                    id("sink"),
                    json!({"mode":"fanout"}),
                    scope(),
                    owner.clone(),
                    None,
                )
                .is_err());
        }
        let producer_owner = Arc::new(QuerySourceProgress::new(GRAPH, id("producer"))?);
        let consumers = owners();
        let producer = factory(&plugin, ComponentRole::Transformer)
            .create_component_with_input_progress(
                id("producer"),
                json!({"mode":"fanout"}),
                scope(),
                producer_owner.clone(),
                Some(consumers.clone()),
            )?;
        assert_eq!(
            producer.recovery_contract(),
            ComponentRecovery::default()
                .with_consumer_progress(consumers)
                .with_volatile_input_progress(producer_owner)
        );
    }
    for factory in super::plugin()
        .factories()
        .iter()
        .filter(|factory| factory.metadata().role == ComponentRole::Service)
    {
        assert!(!factory
            .descriptor()
            .dependencies
            .contains_key("input_progress"));
        assert!(factory
            .create_component_with_input_progress(
                id("sink"),
                json!({}),
                scope(),
                Arc::new(QuerySourceProgress::new(GRAPH, id("sink"))?),
                None,
            )
            .is_err());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn standalone_transaction_capable_factories_keep_volatile_handling_separate_from_atomic_steps(
) -> anyhow::Result<()> {
    for plugin in [
        super::plugin(),
        NativePlugin::from_definition(super::definition()?)?,
    ] {
        let factory = plugin
            .factories()
            .iter()
            .find(|factory| {
                factory.metadata().role == ComponentRole::Transformer
                    && factory.metadata().capabilities.transactional
            })
            .expect("transaction-capable factory");
        assert!(factory
            .descriptor()
            .dependencies
            .contains_key("input_progress"));
        let owner = Arc::new(QuerySourceProgress::new(GRAPH, id("standalone"))?);
        let mut component = factory.create_component_with_input_progress(
            id("standalone"),
            json!({}),
            scope(),
            owner.clone(),
            None,
        )?;
        assert_eq!(
            component.recovery_contract(),
            ComponentRecovery::default().with_volatile_input_progress(owner.clone()),
        );
        component.start().await?;
        let output = component
            .transform(InputEnvelope {
                port: PortId::try_new("in")?,
                envelope: GraphChangeCodec::encode_changes(
                    &[],
                    StreamId::try_new("input/out")?,
                    1,
                    None,
                )?,
            })
            .await?;
        assert_eq!(output.len(), 1);
        component.delivery_completed(&output).await?;
        component.stop().await?;
        assert!(!owner.snapshot().ready && owner.snapshot().checkpoints.is_empty());
        assert!(
            component.into_transactional().is_err(),
            "an actual input tracker cannot become an atomic participant"
        );
        assert!(
            factory
                .create_component(id("step"), json!({}))?
                .into_transactional()
                .is_ok(),
            "the factory's separate transactional mode remains supported"
        );
    }
    Ok(())
}

mod graph_fanout {
    use super::*;
    use anyhow::Context;
    use drasi_computation_contracts::exchange::ResultDiff;
    use std::collections::BTreeMap;
    use tokio::sync::mpsc;

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum BindingCase {
        Tracked,
        Substitute,
        Untracked,
        Accepted,
    }

    struct Input {
        descriptor: ComponentDescriptor,
        receiver: mpsc::Receiver<ChangeEnvelope>,
        stops: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ComputationComponent for Input {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.descriptor
        }
        async fn start(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            self.receiver.close();
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[async_trait]
    impl EnvelopeSource for Input {
        async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
            Ok(self.receiver.recv().await.map(|envelope| OutputEnvelope {
                port: PortId::try_new("out").expect("fixture port"),
                envelope,
            }))
        }
    }

    struct Capture {
        descriptor: ComponentDescriptor,
        sender: Option<mpsc::Sender<ChangeEnvelope>>,
        stops: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ComputationComponent for Capture {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.descriptor
        }
        async fn start(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            self.sender.take();
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[async_trait]
    impl EnvelopeSink for Capture {
        fn completion(&self) -> SinkCompletion {
            SinkCompletion::Handled
        }
        async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
            self.sender
                .as_ref()
                .context("capture is stopped")?
                .send(input.envelope)
                .await?;
            Ok(())
        }
    }

    fn endpoint(component: &str, port: &str) -> Endpoint {
        Endpoint::new(id(component), PortId::try_new(port).expect("fixture port"))
    }

    fn input(sequence: u64) -> anyhow::Result<ChangeEnvelope> {
        let metadata = ElementMetadata {
            reference: ElementReference::new("input", "one"),
            labels: Arc::from([Arc::from("Item")]),
            effective_from: sequence,
        };
        let change = match sequence {
            1 => SourceChange::Insert {
                element: Element::Node {
                    metadata,
                    properties: ElementPropertyMap::from(json!({"value":10})),
                },
            },
            2 => SourceChange::Update {
                element: Element::Node {
                    metadata,
                    properties: ElementPropertyMap::from(json!({"value":20})),
                },
            },
            3 => SourceChange::Delete { metadata },
            _ => anyhow::bail!("unexpected fixture input sequence"),
        };
        Ok(GraphChangeCodec::encode_change(
            change,
            StreamId::try_new("input/out")?,
            sequence,
            None,
        )?)
    }

    fn check_query_effect(
        envelope: &ChangeEnvelope,
        query: &str,
        sequence: u64,
        signatures: &mut BTreeMap<String, u64>,
    ) -> anyhow::Result<Value> {
        let result = QueryChangeCodec::to_legacy_result(envelope)?;
        anyhow::ensure!(
            result.query_id == query && result.sequence == sequence,
            "{result:?}"
        );
        let (before, after) = if query == "first" {
            (json!({"value":10}), json!({"value":20}))
        } else {
            (json!({"scaled":100}), json!({"scaled":200}))
        };
        let signature = match (sequence, result.results.as_slice()) {
            (
                1,
                [ResultDiff::Add {
                    data,
                    row_signature,
                }],
            ) if data == &before => *row_signature,
            (
                2,
                [ResultDiff::Update {
                    before: old,
                    after: new,
                    row_signature,
                    ..
                }],
            ) if old == &before && new == &after => *row_signature,
            (
                3,
                [ResultDiff::Delete {
                    data,
                    row_signature,
                }],
            ) if data == &after => *row_signature,
            _ => anyhow::bail!("unexpected independent query effect: {result:?}"),
        };
        if let Some(previous) = signatures.insert(query.into(), signature) {
            anyhow::ensure!(previous == signature, "query row identity changed");
        }
        Ok(serde_json::to_value(result.results)?)
    }

    async fn graph_case(
        native: &NativePlugin,
        instance: &str,
        binding: BindingCase,
    ) -> anyhow::Result<()> {
        let core = drasi_lib::DrasiLib::builder()
            .with_id(instance)
            .build()
            .await?;
        let graph_control = core.computation_control()?;
        let graph_id = graph_control.desired_snapshot().id.to_string();
        let initial_nodes = graph_control.desired_snapshot().nodes.len();
        let source_stops = Arc::new(AtomicUsize::new(0));
        let mut captures = Vec::new();
        let mut native_sinks = Vec::new();
        let mut remote = None;
        let mut sender = None;
        let result = tokio::time::timeout(TIMEOUT, async {
            let owners: Vec<_> = ["first", "second"]
                .into_iter()
                .map(|name| QuerySourceProgress::new(graph_id.clone(), id(name)).map(Arc::new))
                .collect::<std::result::Result<_, _>>()?;
            anyhow::ensure!(!Arc::ptr_eq(&owners[0], &owners[1]));
            let bound_second = if binding == BindingCase::Substitute {
                Arc::new(QuerySourceProgress::new(graph_id.clone(), id("second"))?)
            } else {
                owners[1].clone()
            };
            let producer_owner =
                Arc::new(QuerySourceProgress::new(graph_id.clone(), id("producer"))?);
            let sink_owners: Vec<_> = (0..2)
                .map(|index| {
                    QuerySourceProgress::new(graph_id.clone(), id(&format!("native-sink-{index}")))
                        .map(Arc::new)
                })
                .collect::<std::result::Result<_, _>>()?;
            let mut members = vec![owners[0].clone(), bound_second, sink_owners[1].clone()];
            if matches!(binding, BindingCase::Tracked | BindingCase::Substitute) {
                members.push(sink_owners[0].clone());
            }
            let progress = Arc::new(ConsumerProgressSet::new(
                graph_id.clone(),
                id("producer"),
                members,
            )?);
            let proxy = factory(native, ComponentRole::Transformer)
                .create_component_with_input_progress(
                    id("producer"),
                    json!({"mode":"fanout"}),
                    sdk::Scope {
                        instance_id: instance.into(),
                        graph_id: graph_id.clone(),
                        generation: 1,
                    },
                    producer_owner.clone(),
                    Some(progress.clone()),
                )?;
            remote = Some(proxy.inner.clone());
            let (send, receive) = mpsc::channel(1);
            sender = Some(send);
            let mut builder = ComputationGraph::builder(graph_id.as_str())
                .source(Box::new(Input {
                    descriptor: ComponentDescriptor::try_new(
                        id("input"),
                        vec![super::super::port(PortDirection::Output)],
                    )?,
                    receiver: receive,
                    stops: source_stops.clone(),
                }))
                .transformer(Box::new(proxy))
                .bind_stream(endpoint("input", "out"), StreamId::try_new("input/out")?)
                .bind_stream(
                    endpoint("producer", "out"),
                    StreamId::try_new("producer/out")?,
                )
                .connect(
                    EdgeDefinition::new(endpoint("input", "out"), endpoint("producer", "in")),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                );
            let mut results = Vec::new();
            for (owner, text) in owners.iter().zip([
                "MATCH (n:Item) RETURN n.value AS value",
                "MATCH (n:Item) RETURN n.value * 10 AS scaled",
            ]) {
                let name = owner.component_id().as_str();
                let query = ContinuousQueryTransformer::new(
                    ContinuousQueryDefinition {
                        graph_id: graph_id.clone(),
                        id: owner.component_id().clone(),
                        query: text.into(),
                        language: ComputationQueryLanguage::Cypher,
                        output_stream: StreamId::try_new(format!("{name}/out"))?,
                        outbox_capacity: NonZeroUsize::new(4).expect("fixture capacity"),
                    },
                    Arc::new(InMemoryComputationProvider),
                )
                .await?
                .with_source_progress(owner.clone())?;
                results.push((name.to_owned(), query.results()));
                builder = builder
                    .query(Box::new(query))
                    .bind_stream(
                        endpoint(name, "out"),
                        StreamId::try_new(format!("{name}/out"))?,
                    )
                    .connect(
                        EdgeDefinition::new(endpoint("producer", "out"), endpoint(name, "in")),
                        Box::new(BoundedPipeConfig { capacity: 1 }),
                    );
            }
            for (index, owner) in sink_owners.iter().enumerate() {
                let name = owner.component_id().as_str();
                let completion = if index == 0 && binding == BindingCase::Accepted {
                    SinkCompletion::Accepted
                } else {
                    SinkCompletion::Handled
                };
                let factory = sink_factory(native, completion);
                let sink = if index == 0
                    && matches!(binding, BindingCase::Untracked | BindingCase::Accepted)
                {
                    factory
                        .create_component(owner.component_id().clone(), json!({"mode":"fanout"}))?
                } else {
                    factory.create_component_with_input_progress(
                        owner.component_id().clone(),
                        json!({"mode":"fanout"}),
                        sdk::Scope {
                            instance_id: instance.into(),
                            graph_id: graph_id.clone(),
                            generation: 1,
                        },
                        owner.clone(),
                        None,
                    )?
                };
                native_sinks.push((name.to_owned(), sink.inner.clone()));
                builder = builder.sink(Box::new(sink)).connect(
                    EdgeDefinition::new(endpoint("producer", "out"), endpoint(name, "in")),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                );
            }
            for upstream in ["first", "second"] {
                let schema = QueryChangeCodec::schema();
                for subscriber in 0..2 {
                    let name = format!("{upstream}-sink-{subscriber}");
                    let (send, receive) = mpsc::channel(4);
                    let stops = Arc::new(AtomicUsize::new(0));
                    builder = builder
                        .sink(Box::new(Capture {
                            descriptor: ComponentDescriptor::try_new(
                                id(&name),
                                vec![PortDescriptor::new(
                                    PortId::try_new("in")?,
                                    PortDirection::Input,
                                    schema.descriptor().clone(),
                                    PipeRequirements::default(),
                                )],
                            )?,
                            sender: Some(send),
                            stops: stops.clone(),
                        }))
                        .connect(
                            EdgeDefinition::new(endpoint(upstream, "out"), endpoint(&name, "in")),
                            Box::new(BoundedPipeConfig { capacity: 1 }),
                        );
                    captures.push((upstream, name, receive, stops));
                }
            }
            let batch = builder.build_components();
            if binding != BindingCase::Tracked {
                let rejection = match batch {
                    Err(error) => format!("{error:#}"),
                    Ok(_) => {
                        anyhow::bail!("invalid native input boundary was accepted: {binding:?}")
                    }
                };
                let expected = if binding == BindingCase::Substitute {
                    "second's actual committed-input owner"
                } else {
                    "does not cover required branch native-sink-0"
                };
                anyhow::ensure!(
                    rejection.contains(expected),
                    "unexpected rejection: {rejection}"
                );
                return Ok::<_, anyhow::Error>(());
            }
            let report = core.add_components(batch?).await?;
            anyhow::ensure!(report.committed, "{report:?}");
            anyhow::ensure!(graph_control.desired_snapshot().nodes.len() == initial_nodes + 10);
            core.start().await?;
            for name in ["input", "producer", "first", "second"]
                .into_iter()
                .chain(captures.iter().map(|(_, name, _, _)| name.as_str()))
                .chain(native_sinks.iter().map(|(name, _)| name.as_str()))
            {
                core.computation_component(name)?.wait_started().await?;
            }
            let mut updates = progress.read_only().subscribe()?;
            let mut input_updates = producer_owner.subscribe();
            anyhow::ensure!(updates
                .snapshot()?
                .consumers
                .values()
                .all(|state| state.checkpoints.is_empty()));
            let mut signatures = BTreeMap::new();
            for sequence in 1..=3 {
                sender
                    .as_ref()
                    .context("input sender")?
                    .send(input(sequence)?)
                    .await?;
                let mut branch_effects = BTreeMap::new();
                for (upstream, name, receiver, _) in &mut captures {
                    let envelope = receiver
                        .recv()
                        .await
                        .with_context(|| format!("sink {name} stopped early"))?;
                    let effect =
                        check_query_effect(&envelope, upstream, sequence, &mut signatures)?;
                    if let Some(previous) = branch_effects.insert(*upstream, effect.clone()) {
                        anyhow::ensure!(
                            previous == effect,
                            "query subscribers received different effects"
                        );
                    }
                }
                loop {
                    let snapshot = updates.snapshot()?;
                    let input = input_updates.borrow_and_update().clone();
                    let handled = |state: &SourceProgressSnapshot| {
                        state.checkpoints.len() == 1
                            && !state.persistent
                            && state
                                .checkpoints
                                .values()
                                .all(|position| position.sequence == sequence)
                    };
                    if snapshot.consumers.values().all(|state| handled(state)) && handled(&input) {
                        break;
                    }
                    tokio::select! {
                        result = updates.changed() => result?,
                        result = input_updates.changed() => result?,
                    }
                }
                for (_, sink) in &native_sinks {
                    sink.unit(
                        abi::operation::CONTROL,
                        &command(
                            "expect-handled",
                            json!({
                                "starts":1, "stops":0, "effects":effects(sequence),
                            }),
                        )?,
                    )
                    .await?;
                }
                for (name, results) in &results {
                    let rows = results
                        .snapshot()?
                        .rows
                        .values()
                        .map(|row| {
                            Ok(QueryChangeCodec::row_values_to_json(
                                &QueryChangeCodec::decode_row(row)?.values,
                            ))
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    let expected = match (name.as_str(), sequence) {
                        ("first", 1) => vec![json!({"value":10})],
                        ("first", 2) => vec![json!({"value":20})],
                        ("second", 1) => vec![json!({"scaled":100})],
                        ("second", 2) => vec![json!({"scaled":200})],
                        (_, 3) => vec![],
                        _ => unreachable!(),
                    };
                    anyhow::ensure!(rows == expected, "{name}: {rows:?} != {expected:?}");
                }
            }
            remote
                .as_ref()
                .context("native producer")?
                .unit(
                    abi::operation::CONTROL,
                    &command(
                        "expect",
                        fixture::snapshot_json(&progress.read_only().snapshot()?)?,
                    )?,
                )
                .await?;
            for (_, name, _, _) in &captures {
                anyhow::ensure!(core
                    .computation_component(name)?
                    .observed()?
                    .failure
                    .is_none());
            }
            Ok(())
        })
        .await;
        let cleanup = tokio::time::timeout(TIMEOUT, core.shutdown()).await;
        result.context("native transformer graph qualification timed out")??;
        cleanup.context("DrasiLib shutdown timed out")??;
        let remote = remote.context("native producer was not constructed")?;
        if binding == BindingCase::Tracked {
            remote
                .unit(
                    abi::operation::CONTROL,
                    &command(
                        "expect-calls",
                        json!({
                            "starts":1, "stops":1, "transforms":3, "completions":3,
                        }),
                    )?,
                )
                .await?;
            anyhow::ensure!(source_stops.load(Ordering::SeqCst) == 1);
            for (_, name, receiver, stops) in &mut captures {
                anyhow::ensure!(
                    stops.load(Ordering::SeqCst) == 1,
                    "sink {name} did not stop exactly once"
                );
                anyhow::ensure!(
                    tokio::time::timeout(TIMEOUT, receiver.recv())
                        .await?
                        .is_none(),
                    "sink {name} received duplicate handling"
                );
            }
            anyhow::ensure!(sender
                .context("input sender")?
                .send(input(1)?)
                .await
                .is_err());
            for (_, sink) in &native_sinks {
                sink.unit(
                    abi::operation::CONTROL,
                    &command(
                        "expect-handled",
                        json!({
                            "starts":1, "stops":1, "effects":effects(3),
                        }),
                    )?,
                )
                .await?;
            }
        }
        closed(
            &remote
                .unit(abi::operation::CONTROL, &command("snapshot", Value::Null)?)
                .await
                .unwrap_err(),
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_input_progress_proxy_never_publishes_on_its_own() -> anyhow::Result<()> {
        for native in [
            plugin()?,
            super::super::super::factory::recovery_tests::sqlite_plugin()?,
        ] {
            let owner = Arc::new(QuerySourceProgress::new(GRAPH, id("sink"))?);
            let before = sdk::progress::encode_snapshot(&owner.snapshot())?;
            let mut sink = sink_factory(&native, SinkCompletion::Handled)
                .create_component_with_input_progress(
                    id("sink"),
                    json!({"mode":"fanout"}),
                    scope(),
                    owner.clone(),
                    None,
                )?;
            assert!(sink.recovery_contract().replay_retention().is_none());
            sink.start().await?;
            sink.handle(InputEnvelope {
                port: PortId::try_new("in")?,
                envelope: input(1)?,
            })
            .await?;
            sink.stop().await?;
            assert_eq!(sdk::progress::encode_snapshot(&owner.snapshot())?, before);
            sink.inner
                .unit(
                    abi::operation::CONTROL,
                    &command(
                        "expect-handled",
                        json!({
                            "starts":1, "stops":1, "effects":effects(1),
                        }),
                    )?,
                )
                .await?;
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_input_progress_factory_resolves_a_real_checkpoint_resource(
    ) -> anyhow::Result<()> {
        for native in [
            plugin()?,
            super::super::super::factory::recovery_tests::sqlite_plugin()?,
        ] {
            for wrong_type in [false, true] {
                let factory = sink_factory(&native, SinkCompletion::Handled);
                let owner = Arc::new(QuerySourceProgress::new(GRAPH, id("sink"))?);
                let resource = ResourceId::try_new("input-progress")?;
                let mut specification =
                    factory.specification(id("sink"), json!({"mode":"fanout"}))?;
                specification
                    .dependencies
                    .insert("input_progress".into(), vec![resource.clone()]);
                let handle = if wrong_type {
                    ResourceHandle::new(ResourceRole::Checkpoint, owner.clone())
                } else {
                    ResourceHandle::new(
                        ResourceRole::Checkpoint,
                        Arc::new(QuerySourceProgressResource(owner.clone())),
                    )
                };
                let (send, receive) = mpsc::channel(1);
                let graph = ComputationGraph::builder(GRAPH)
                    .declare_resource(ResourceSpecification {
                        id: resource.clone(),
                        role: ResourceRole::Checkpoint,
                        ownership: ResourceOwnership::Borrowed,
                        binding: "input-progress".into(),
                    })?
                    .provide_resource(resource, handle)?
                    .source(Box::new(Input {
                        descriptor: ComponentDescriptor::try_new(
                            id("input"),
                            vec![super::super::port(PortDirection::Output)],
                        )?,
                        receiver: receive,
                        stops: Arc::new(AtomicUsize::new(0)),
                    }))
                    .component(specification, factory)
                    .bind_stream(endpoint("input", "out"), StreamId::try_new("input/out")?)
                    .connect(
                        EdgeDefinition::new(endpoint("input", "out"), endpoint("sink", "in")),
                        Box::new(BoundedPipeConfig { capacity: 1 }),
                    )
                    .build();
                if wrong_type {
                    assert!(graph.is_err());
                    continue;
                }
                let mut graph = graph?;
                let mut updates = owner.subscribe();
                let run = graph.start()?;
                let control = run.control();
                let result = tokio::time::timeout(TIMEOUT, async {
                    tokio::join!(run, async {
                        let result = async {
                            send.send(input(1)?).await?;
                            loop {
                                let state = updates.borrow_and_update().clone();
                                if state.checkpoints.len() == 1
                                    && state
                                        .checkpoints
                                        .values()
                                        .all(|checkpoint| checkpoint.sequence == 1)
                                {
                                    anyhow::ensure!(!state.persistent && state.ready);
                                    return Ok::<_, anyhow::Error>(());
                                }
                                updates.changed().await?;
                            }
                        }
                        .await;
                        control.cancel();
                        result
                    })
                })
                .await;
                let cleanup = graph.shutdown().await;
                let (run, observed) = result?;
                observed?;
                assert!(matches!(run, Ok(()) | Err(GraphError::Cancelled)));
                cleanup?;
            }
        }
        Ok(())
    }

    async fn qualify(flavor: &str) -> anyhow::Result<()> {
        for (linked, native) in [
            (true, plugin()?),
            (
                false,
                super::super::super::factory::recovery_tests::sqlite_plugin()?,
            ),
        ] {
            for binding in [
                BindingCase::Tracked,
                BindingCase::Substitute,
                BindingCase::Untracked,
                BindingCase::Accepted,
            ] {
                graph_case(
                    &native,
                    &format!("native-transformer-{flavor}-{linked}-{binding:?}"),
                    binding,
                )
                .await
                .with_context(|| {
                    format!("linked={linked}, binding={binding:?}, runtime={flavor}")
                })?;
            }
        }
        Ok(())
    }

    fn effects(count: u64) -> Vec<Value> {
        [
            json!({"operation":"insert", "value":10, "sequence":1}),
            json!({"operation":"update", "value":20, "sequence":2}),
            json!({"operation":"delete", "value":null, "sequence":3}),
        ]
        .into_iter()
        .take(count as usize)
        .collect()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn consumer_progress_native_transformer_graph_fanout_current_thread() -> anyhow::Result<()>
    {
        qualify("current").await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn consumer_progress_native_transformer_graph_fanout_multi_thread() -> anyhow::Result<()>
    {
        qualify("multi").await
    }
}
