// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use async_trait::async_trait;
use drasi_lib::{computation::v1::*, management::*, DrasiLib, Query};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig};
use futures::StreamExt;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

fn name(value: &str) -> ComponentName {
    ComponentName::try_new(value).unwrap()
}

fn source(id: ComponentId) -> anyhow::Result<ApplicationSource> {
    Ok(ApplicationSource::new(
        id.to_string(),
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )?
    .0)
}

#[tokio::test]
async fn ordinary_graph_inspection_and_events_work_without_an_observer_source() -> anyhow::Result<()>
{
    let core = DrasiLib::builder().build().await?;
    let mut events = core.subscribe_all_component_events();
    let source = core.create_source("orders", true, source).await?;
    source.wait_created().await?;
    let event = tokio::time::timeout(Duration::from_secs(10), events.recv()).await??;
    assert_eq!(event.component_id, source.id().as_str());
    assert_eq!(event.status, drasi_lib::ComponentStatus::Added);
    core.rename_component(source.id().as_str(), "purchases")
        .await?;
    let snapshot = core.get_graph().await?;
    assert_eq!(snapshot.nodes.len(), 2);
    let node = snapshot.get_component(source.id().as_str()).unwrap();
    assert_eq!(node.kind, drasi_lib::ComponentKind::Source);
    assert_eq!(
        node.metadata.get("name").map(String::as_str),
        Some("purchases")
    );
    let control = core.computation_control()?;
    assert_eq!(control.desired_snapshot().nodes.len(), 1);
    assert_eq!(
        control.desired_snapshot().nodes[0].descriptor.id(),
        source.id()
    );
    core.rename_component(source.id().as_str(), "renamed")
        .await?;
    let snapshot = core.component_graph().snapshot().await?;
    assert_eq!(
        snapshot
            .get_component(source.id().as_str())
            .unwrap()
            .metadata
            .get("name")
            .map(String::as_str),
        Some("renamed")
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn empty_instance_stays_live_without_implicit_components() -> anyhow::Result<()> {
    let core = DrasiLib::builder().build().await?;
    let control = core.computation_control()?;
    assert!(core.list_sources().await?.is_empty());
    assert!(control.desired_snapshot().nodes.is_empty());
    assert!(core.snapshot_configuration().await?.sources.is_empty());
    let snapshot = core.get_graph().await?;
    assert_eq!(snapshot.nodes.len(), 1);
    assert_eq!(snapshot.nodes[0].kind, drasi_lib::ComponentKind::Instance);
    assert!(snapshot.edges.is_empty());

    core.start().await?;
    let first = core.create_source("orders", true, source).await?;
    tokio::time::timeout(Duration::from_secs(10), first.wait_started()).await??;
    core.remove_source(first.id().as_str(), false).await?;
    assert!(control.desired_snapshot().nodes.is_empty());
    assert!(control.observed().components.is_empty());
    assert!(first.observed().is_err());
    core.stop().await?;
    core.start().await?;

    let replacement = core.create_source("orders", true, source).await?;
    tokio::time::timeout(Duration::from_secs(10), replacement.wait_started()).await??;
    assert_ne!(replacement.id(), first.id());
    assert!(first.observed().is_err());
    assert_eq!(core.list_sources().await?.len(), 1);
    assert_eq!(
        core.snapshot_configuration().await?.sources[0].id,
        replacement.id().as_str()
    );
    core.remove_source(replacement.id().as_str(), false).await?;
    assert!(control.desired_snapshot().nodes.is_empty());
    core.shutdown().await?;
    Ok(())
}

fn external(error: &drasi_lib::DrasiError) -> bool {
    error
        .downcast_ref::<GraphError>()
        .is_some_and(|error| matches!(cause(error), GraphError::ExternalComponentId { .. }))
}

fn cause(error: &GraphError) -> &GraphError {
    match error {
        GraphError::AdditionRejected { cause: error, .. } => cause(error),
        GraphError::Reported { cause: error } => cause(error),
        other => other,
    }
}

struct Service {
    descriptor: ComponentDescriptor,
}

struct NativeFeed(ComponentDescriptor);

#[async_trait]
impl ComputationComponent for NativeFeed {
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
impl EnvelopeSource for NativeFeed {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

#[async_trait]
impl ComputationComponent for Service {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ComputationService for Service {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

fn service(id: ComponentId, label: &str) -> ComponentAddition {
    ComponentAddition::new(
        name(label),
        ConstructedComponent::service(Box::new(Service {
            descriptor: ComponentDescriptor::try_new(id, vec![]).unwrap(),
        })),
    )
    .auto_start(false)
}

struct ServiceFactory(FactoryDescriptor);

impl ServiceFactory {
    fn new() -> Arc<Self> {
        Arc::new(Self(FactoryDescriptor {
            implementation: ImplementationIdentity::try_new("fixture/identity-service", "1")
                .unwrap(),
            role: ComponentRole::Service,
            configuration_version: 1,
            configuration: ConfigurationSchema {
                fields: BTreeMap::new(),
                allow_additional: false,
            },
            dependencies: BTreeMap::new(),
        }))
    }

    fn definition(&self, label: &str) -> ComponentDefinition {
        ComponentDefinition {
            name: name(label),
            id: None,
            implementation: self.0.implementation.clone(),
            configuration: serde_json::json!({}),
            dependencies: BTreeMap::new(),
            streams: BTreeMap::new(),
            lifecycle: LifecyclePolicy { auto_start: false },
        }
    }

    fn specification(&self, id: ComponentId) -> GraphResult<ComponentSpecification> {
        Ok(ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(id, vec![])?,
            role: ComponentRole::Service,
            completion: None,
            implementation: self.0.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        })
    }
}

#[async_trait]
impl ComponentFactory for ServiceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.0
    }
    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }
    fn specification_from_config(
        &self,
        id: ComponentId,
        _: serde_json::Value,
    ) -> anyhow::Result<ComponentSpecification> {
        Ok(self.specification(id)?)
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        Ok(ConstructedComponent::service(Box::new(Service {
            descriptor: context.specification.descriptor.clone(),
        })))
    }
}

#[tokio::test]
async fn owned_native_admission_resolves_ordinary_named_inputs_without_a_background_mirror(
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let core = DrasiLib::builder()
            .with_allow_external_component_ids(true)
            .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
            .build()
            .await?;
        let query = core
            .add_query_with_handle(
                Query::cypher("view")
                    .query("MATCH (n) RETURN n")
                    .from_input("native")
                    .enable_bootstrap(false)
                    .auto_start(false)
                    .build()?,
            )
            .await?;
        let id = ComponentId::generate();
        let port = PortId::try_new("out")?;
        let descriptor = ComponentDescriptor::try_new(
            id.clone(),
            vec![PortDescriptor::new(
                port.clone(),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?;
        core.add_computation_component(
            ComponentAddition::new(
                name("native"),
                ConstructedComponent::source(Box::new(NativeFeed(descriptor))),
            )
            .bind_stream(port, StreamId::try_new("native-stream")?)
            .auto_start(false),
        )
        .await?;
        core.computation_component(query.id().as_str())?
            .wait_created()
            .await?;
        assert_eq!(
            core.get_query_config(query.id().as_str()).await?.inputs[0]
                .component
                .as_id(),
            Some(&id)
        );
        core.rename_component(id.as_str(), "renamed-native").await?;
        assert_eq!(
            core.get_query_config(query.id().as_str()).await?.inputs[0]
                .component
                .as_id(),
            Some(&id)
        );
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn default_creation_owns_uuid_and_rejects_all_external_admission_paths() -> anyhow::Result<()>
{
    let core = DrasiLib::builder()
        .with_id("not-a-component-uuid")
        .build()
        .await?;
    assert!(!core.allow_external_component_ids());
    assert!(!core.computation_control()?.allows_external_component_ids());
    let created = core.create_source("orders", true, source).await?;
    created.wait_created().await?;
    assert_eq!(core.component_by_name("orders")?.id(), created.id());
    assert_eq!(
        core.get_source_info(created.id().as_str()).await?.name,
        "orders"
    );
    let external_id = ComponentId::generate();
    assert!(external(
        &core
            .add_source("external", source(external_id.clone())?)
            .await
            .unwrap_err()
    ));
    assert!(external(
        &core
            .add_computation_component(service(external_id.clone(), "external"))
            .await
            .err()
            .unwrap()
    ));
    let control = core.computation_control()?;
    let error = control
        .add_component(service(external_id.clone(), "direct"))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("managed configuration"));
    let batch = ComponentBatch::builder()
        .service(
            name("batch"),
            Box::new(Service {
                descriptor: ComponentDescriptor::try_new(external_id.clone(), vec![])?,
            }),
        )
        .build()?;
    assert!(external(&core.add_components(batch).await.unwrap_err()));
    let declaration = service(external_id, "reconcile").definition;
    let preview = control
        .preview(
            control.desired_snapshot().revision,
            vec![DesiredMutation::PutComponent(declaration)],
        )
        .await?;
    assert!(control
        .reconcile(preview, TopologyBindings::default())
        .await
        .unwrap_err()
        .to_string()
        .contains("managed configuration"));
    let substitute = ComponentId::generate();
    let invalid = core
        .create_source("substitute", false, move |_| source(substitute))
        .await?;
    assert!(invalid.wait_created().await.is_err());
    let id = created.id().clone();
    core.remove_source(id.as_str(), false).await?;
    assert!(created.observed().is_err());
    assert!(external(
        &core.add_source("orders", source(id)?).await.unwrap_err()
    ));
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn opt_in_preserves_external_ids_and_conflicts_keep_existing_metadata() -> anyhow::Result<()>
{
    let id = ComponentId::generate();
    let core = DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_source("orders", source(id.clone())?)
        .build()
        .await?;
    assert_eq!(core.component_by_name("orders")?.id(), &id);
    for (other, label, same_id) in
        [(id.clone(), "other", true), (ComponentId::generate(), "orders", false)]
    {
        let error = core.add_source(label, source(other)?).await.unwrap_err();
        let conflict = match (same_id, cause(error.downcast_ref::<GraphError>().unwrap())) {
            (true, GraphError::DuplicateComponentId { existing })
            | (false, GraphError::DuplicateComponentName { existing }) => existing,
            _ => panic!("missing structured conflict: {error:#}"),
        };
        assert_eq!(conflict.id, id);
        assert_eq!(conflict.name, name("orders"));
    }
    let old = core.computation_component(id.as_str())?;
    core.remove_source(id.as_str(), false).await?;
    core.add_source("orders", source(id.clone())?).await?;
    assert!(old.observed().is_err());
    assert_eq!(core.component_by_name("orders")?.id(), &id);
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn scoped_native_factory_and_batch_permissions_cannot_be_reused() -> anyhow::Result<()> {
    let core = DrasiLib::builder().build().await?;
    let factory = ServiceFactory::new();
    core.register_component_factory(factory.clone()).await?;
    let created = core
        .create_computation_component(factory.definition("native"))
        .await?;
    created.wait_created().await?;
    let old_id = created.id().clone();
    let report = core
        .create_components(move |ids| {
            let id = ids.allocate()?;
            ComponentBatch::builder()
                .component(name("batch"), factory.specification(id)?, factory.clone())
                .build()
        })
        .await?;
    let assigned = &report.creation.as_ref().unwrap().components[0];
    assert_eq!(assigned.name.as_str(), "batch");
    core.reconcile_desired_state().await?;
    core.computation_component(assigned.id.as_str())?
        .wait_created()
        .await?;
    assert!(core
        .create_components(move |_| {
            ComponentBatch::builder()
                .service(
                    name("copied"),
                    Box::new(Service {
                        descriptor: ComponentDescriptor::try_new(old_id, vec![])?,
                    }),
                )
                .build()
        })
        .await
        .is_err());
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn reaction_factory_waits_for_names_and_keeps_each_uuid_binding() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let core = DrasiLib::builder()
            .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
            .build()
            .await?;
        let received = Arc::new(Mutex::new(Vec::new()));
        let captured = received.clone();
        let reaction = core
            .create_reaction_with_inputs(
                "consumer",
                vec![
                    drasi_lib::config::ComponentReference::by_name("view")?,
                    drasi_lib::config::ComponentReference::by_name("later")?,
                ],
                false,
                move |id, inputs| {
                    *captured.lock().unwrap() = inputs.clone();
                    Ok(drasi_reaction_application::ApplicationReaction::new(
                        id.to_string(),
                        inputs.iter().map(ToString::to_string).collect(),
                    )
                    .0)
                },
            )
            .await?;
        assert!(received.lock().unwrap().is_empty());
        assert!(reaction
            .observed()?
            .waiting_for
            .iter()
            .any(|wait| matches!(wait,
            DependencyWait::ComponentName { name } if name.as_str() == "view")));
        let first = core
            .add_query_with_handle(
                Query::cypher("view")
                    .query("MATCH (n) RETURN n")
                    .enable_bootstrap(false)
                    .auto_start(false)
                    .build()?,
            )
            .await?;
        first.wait_created().await?;
        assert_eq!(
            core.get_reaction_info(reaction.id().as_str())
                .await?
                .queries[0]
                .as_id(),
            Some(first.id())
        );
        core.rename_component(first.id().as_str(), "renamed-view")
            .await?;
        core.add_query(
            Query::cypher("view")
                .query("MATCH (n) RETURN n")
                .enable_bootstrap(false)
                .auto_start(false)
                .build()?,
        )
        .await?;
        let second = core
            .add_query_with_handle(
                Query::cypher("later")
                    .query("MATCH (n) RETURN n")
                    .enable_bootstrap(false)
                    .auto_start(false)
                    .build()?,
            )
            .await?;
        second.wait_created().await?;
        reaction.wait_created().await?;
        assert_eq!(
            *received.lock().unwrap(),
            vec![first.id().clone(), second.id().clone()]
        );
        let topology = core.computation_control()?.desired_snapshot();
        assert!(topology
            .subscriptions
            .contains(&(first.id().clone(), reaction.id().clone())));
        core.rename_component(reaction.id().as_str(), "renamed-consumer")
            .await?;
        assert_eq!(
            core.get_reaction_info(reaction.id().as_str()).await?.name,
            "renamed-consumer"
        );
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn named_query_inputs_wait_then_bind_once_across_rename_and_name_reuse() -> anyhow::Result<()>
{
    let core = DrasiLib::builder()
        .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
        .build()
        .await?;
    let query = core
        .add_query_with_handle(
            Query::cypher("view")
                .query("MATCH (n:Item) RETURN n")
                .from_input("orders")
                .enable_bootstrap(false)
                .auto_start(false)
                .build()?,
        )
        .await?;
    let waiting = query.observed()?;
    assert!(waiting.failure.is_none(), "{waiting:?}");
    assert!(waiting.waiting_for.iter().any(|wait| matches!(wait,
        DependencyWait::ComponentName { name: dependency } if dependency.as_str() == "orders")));
    let upstream = core.create_source("incoming", true, source).await?;
    upstream.wait_created().await?;
    upstream.rename(name("orders")).await?;
    let query_id = query.id().clone();
    let query = core.computation_component(query_id.as_str())?;
    tokio::time::timeout(Duration::from_secs(10), query.wait_created()).await??;
    let bound = core.get_query_config(query_id.as_str()).await?;
    assert_eq!(bound.inputs[0].component.as_id(), Some(upstream.id()));
    let before = core.computation_control()?.desired_snapshot();
    core.rename_component(upstream.id().as_str(), "purchases")
        .await?;
    let replacement = core.create_source("orders", true, source).await?;
    assert_ne!(replacement.id(), upstream.id());
    let after = core.get_query_config(query_id.as_str()).await?;
    assert_eq!(after.inputs[0].component.as_id(), Some(upstream.id()));
    assert_eq!(
        before
            .edges
            .iter()
            .map(|edge| &edge.definition)
            .collect::<Vec<_>>(),
        core.computation_control()?
            .desired_snapshot()
            .edges
            .iter()
            .map(|edge| &edge.definition)
            .collect::<Vec<_>>()
    );
    core.rename_component(query_id.as_str(), "renamed-view")
        .await?;
    assert_eq!(
        core.get_query_config(query_id.as_str()).await?.name,
        "renamed-view"
    );
    assert_eq!(core.component_by_name("renamed-view")?.id(), &query_id);
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn fresh_instances_allocate_new_ids_and_explicit_query_ids_require_opt_in(
) -> anyhow::Result<()> {
    async fn build() -> anyhow::Result<DrasiLib> {
        Ok(DrasiLib::builder()
            .with_source_factory("orders", true, source)
            .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
            .with_query(
                Query::cypher("view")
                    .query("MATCH (n) RETURN n")
                    .from_input("orders")
                    .auto_start(false)
                    .build()?,
            )
            .with_reaction_inputs_factory(
                "consumer",
                vec![drasi_lib::config::ComponentReference::by_name("view")?],
                false,
                |id, inputs| {
                    Ok(drasi_reaction_application::ApplicationReaction::new(
                        id.to_string(),
                        inputs.iter().map(ToString::to_string).collect(),
                    )
                    .0)
                },
            )
            .build()
            .await?)
    }
    let first = build().await?;
    let second = build().await?;
    assert_ne!(
        first.component_by_name("orders")?.id(),
        second.component_by_name("orders")?.id()
    );
    assert_ne!(
        first.component_by_name("view")?.id(),
        second.component_by_name("view")?.id()
    );
    assert_ne!(
        first.component_by_name("consumer")?.id(),
        second.component_by_name("consumer")?.id()
    );
    let error = first
        .add_query(
            Query::cypher("external")
                .with_id(ComponentId::generate())
                .query("MATCH (n) RETURN n")
                .build()?,
        )
        .await
        .unwrap_err();
    assert!(external(&error));
    first.shutdown().await?;
    second.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn ordinary_processing_continues_under_the_same_uuid_after_rename() -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let core = DrasiLib::builder()
            .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
            .build()
            .await?;
        let input = Arc::new(Mutex::new(None));
        let captured_input = input.clone();
        let upstream = core
            .create_source("events", true, move |id| {
                let (source, handle) = ApplicationSource::new(
                    id.to_string(),
                    ApplicationSourceConfig {
                        properties: Default::default(),
                        durability: None,
                    },
                )?;
                *captured_input.lock().unwrap() = Some(handle);
                Ok(source)
            })
            .await?;
        let query = core
            .add_query_with_handle(
                Query::cypher("view")
                    .query("MATCH (n:Item) RETURN n.value AS value")
                    .from_input("events")
                    .enable_bootstrap(false)
                    .build()?,
            )
            .await?;
        let output = Arc::new(Mutex::new(None));
        let captured_output = output.clone();
        let reaction = core
            .create_reaction_with_inputs(
                "consumer",
                vec![drasi_lib::config::ComponentReference::by_name("view")?],
                true,
                move |id, inputs| {
                    let (reaction, handle) = drasi_reaction_application::ApplicationReaction::new(
                        id.to_string(),
                        inputs.iter().map(ToString::to_string).collect(),
                    );
                    *captured_output.lock().unwrap() = Some(handle);
                    Ok(reaction)
                },
            )
            .await?;
        core.start().await?;
        query.wait_started().await?;
        reaction.wait_started().await?;
        let output = output.lock().unwrap().take().unwrap();
        let mut results = output.take_receiver().await.unwrap();
        let mut subscription = core.subscribe_query_results(query.id().as_str()).await?;
        let input = input.lock().unwrap().take().unwrap();
        input
            .send_node_insert(
                "one",
                vec!["Item"],
                drasi_source_application::PropertyMapBuilder::new()
                    .with_integer("value", 1)
                    .build(),
            )
            .await?;
        subscription.stream.next().await.unwrap()?;
        assert_eq!(results.recv().await.unwrap().query_id, query.id().as_str());
        let revision = core.computation_control()?.desired_snapshot().revision;
        let generation = query.generation();
        core.rename_component(upstream.id().as_str(), "renamed-events")
            .await?;
        core.rename_component(query.id().as_str(), "renamed-view")
            .await?;
        assert!(core.computation_control()?.desired_snapshot().revision > revision);
        input
            .send_node_insert(
                "two",
                vec!["Item"],
                drasi_source_application::PropertyMapBuilder::new()
                    .with_integer("value", 2)
                    .build(),
            )
            .await?;
        subscription.stream.next().await.unwrap()?;
        assert_eq!(results.recv().await.unwrap().query_id, query.id().as_str());
        assert_eq!(core.get_query_results(query.id().as_str()).await?.len(), 2);
        assert_eq!(
            core.component_by_name("renamed-view")?.generation(),
            generation
        );
        assert_eq!(
            core.get_reaction_info(reaction.id().as_str())
                .await?
                .queries[0]
                .as_id(),
            Some(query.id())
        );
        drop(subscription);
        core.stop().await?;
        core.start().await?;
        assert_eq!(core.component_by_name("renamed-view")?.id(), query.id());
        assert_eq!(core.get_query_results(query.id().as_str()).await?.len(), 2);
        core.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[derive(Default)]
struct Store {
    state: Mutex<StoreState>,
    fail_after_commit: AtomicBool,
}

#[derive(Default)]
struct StoreState {
    current: CommittedConfiguration,
    receipts: BTreeMap<String, (DesiredInstance, AcceptanceReceipt)>,
    snapshots: BTreeMap<String, CommittedConfiguration>,
}

struct StoreOwner(Arc<Store>);

#[async_trait]
impl ConfigurationStore for StoreOwner {
    fn guarantees(&self) -> ConfigurationStoreGuarantees {
        ConfigurationStoreGuarantees::DURABLE
    }

    async fn open(&self, _: &str) -> anyhow::Result<Arc<dyn ConfigurationSession>> {
        Ok(self.0.clone())
    }
}

#[async_trait]
impl ConfigurationSession for Store {
    async fn load(&self) -> anyhow::Result<CommittedConfiguration> {
        Ok(self.state.lock().unwrap().current.clone())
    }
    async fn commit(
        &self,
        expected: u64,
        request: &str,
        desired: &DesiredInstance,
        creation: Option<&AcceptedComponentCreation>,
        updates: &[AcceptedComponentUpdate],
    ) -> anyhow::Result<AcceptanceReceipt> {
        let mut state = self.state.lock().unwrap();
        if let Some((prior, receipt)) = state.receipts.get(request) {
            anyhow::ensure!(
                prior == desired
                    && receipt.creation.as_ref() == creation
                    && receipt.updates == updates,
                ManagementError::RequestConflict
            );
            return Ok(receipt.clone());
        }
        anyhow::ensure!(
            expected == state.current.revision,
            ManagementError::RevisionConflict {
                expected,
                actual: state.current.revision
            }
        );
        if state.current.desired != *desired {
            state.current = CommittedConfiguration {
                revision: state.current.revision + 1,
                desired: desired.clone(),
                pending_updates: state.current.pending_updates.clone(),
            };
        }
        state.current.pending_updates.extend_from_slice(updates);
        let receipt = AcceptanceReceipt {
            request_id: request.into(),
            revision: state.current.revision,
            durable: true,
            creation: creation.cloned(),
            updates: updates.to_vec(),
        };
        state
            .receipts
            .insert(request.into(), (desired.clone(), receipt.clone()));
        anyhow::ensure!(
            !self.fail_after_commit.swap(false, Ordering::SeqCst),
            "lost commit acknowledgement"
        );
        Ok(receipt)
    }
    async fn complete_updates(
        &self,
        expected: u64,
        updates: &[ComponentUpdateCompletion],
    ) -> anyhow::Result<CommittedConfiguration> {
        let mut state = self.state.lock().unwrap();
        anyhow::ensure!(
            state.current.revision == expected,
            ManagementError::RevisionConflict {
                expected,
                actual: state.current.revision,
            }
        );
        for update in updates {
            if let Some(pending) = state
                .current
                .pending_updates
                .iter()
                .find(|pending| pending.component_id == update.component_id)
            {
                anyhow::ensure!(
                    pending.operation_id == update.operation_id,
                    ManagementError::RequestConflict
                );
            }
        }
        state.current.pending_updates.retain(|pending| {
            !updates.iter().any(|update| {
                update.component_id == pending.component_id
                    && update.operation_id == pending.operation_id
            })
        });
        Ok(state.current.clone())
    }
    async fn receipt(&self, request: &str) -> anyhow::Result<Option<AcceptanceReceipt>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .receipts
            .get(request)
            .map(|(_, receipt)| receipt.clone()))
    }
    async fn snapshot(&self, name: &str) -> anyhow::Result<CommittedConfiguration> {
        let mut state = self.state.lock().unwrap();
        let current = state.current.clone();
        state.snapshots.insert(name.into(), current.clone());
        Ok(current)
    }
    async fn load_snapshot(&self, name: &str) -> anyhow::Result<Option<CommittedConfiguration>> {
        Ok(self.state.lock().unwrap().snapshots.get(name).cloned())
    }
    async fn close(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

struct Factory {
    descriptor: FactoryDescriptor,
    store: Arc<Store>,
    creations: AtomicUsize,
}

#[async_trait]
impl ComponentFactory for Factory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn specification_from_config(
        &self,
        id: ComponentId,
        _: serde_json::Value,
    ) -> anyhow::Result<ComponentSpecification> {
        Ok(ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(id, vec![])?,
            role: ComponentRole::Service,
            completion: None,
            implementation: self.descriptor.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        })
    }
    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        assert!(self
            .store
            .state
            .lock()
            .unwrap()
            .current
            .desired
            .topology
            .components
            .iter()
            .any(|component| component.descriptor.id() == &context.component_id));
        self.creations.fetch_add(1, Ordering::SeqCst);
        Ok(ConstructedComponent::service(Box::new(Service {
            descriptor: context.specification.descriptor.clone(),
        })))
    }
}

fn definition(factory: &Factory) -> ComponentDefinition {
    ComponentDefinition {
        name: name("managed"),
        id: None,
        implementation: factory.descriptor.implementation.clone(),
        configuration: serde_json::json!({}),
        dependencies: BTreeMap::new(),
        streams: BTreeMap::new(),
        lifecycle: LifecyclePolicy { auto_start: false },
    }
}

async fn managed(store: Arc<Store>, factory: Arc<Factory>) -> anyhow::Result<DrasiLib> {
    let mut factories = FactoryRegistry::default();
    factories.register(factory)?;
    Ok(DrasiLib::builder()
        .with_id("persistent-identity")
        .with_component_factories(factories)
        .with_configuration_store(Arc::new(StoreOwner(store)))
        .build()
        .await?)
}

#[tokio::test]
async fn managed_receipts_and_trusted_restoration_preserve_ids_without_external_adoption(
) -> anyhow::Result<()> {
    let store = Arc::new(Store::default());
    let factory = Arc::new(Factory {
        descriptor: FactoryDescriptor {
            implementation: ImplementationIdentity::try_new("identity/service", "1")?,
            role: ComponentRole::Service,
            configuration_version: 1,
            configuration: ConfigurationSchema {
                fields: BTreeMap::new(),
                allow_additional: false,
            },
            dependencies: BTreeMap::new(),
        },
        store: store.clone(),
        creations: AtomicUsize::new(0),
    });
    let core = managed(store.clone(), factory.clone()).await?;
    store.fail_after_commit.store(true, Ordering::SeqCst);
    assert!(core
        .create_managed_component(0, "create", definition(&factory))
        .await
        .is_err());
    let receipt = core
        .create_managed_component(0, "create", definition(&factory))
        .await?;
    let id = receipt.creation.as_ref().unwrap().components[0].id.clone();
    assert!(receipt.durable);
    core.reconcile_desired_state().await?;
    let old = core.computation_component(id.as_str())?;
    old.wait_created().await?;
    core.snapshot_desired_configuration("saved").await?;
    let saved = core.load_configuration_snapshot("saved").await?.unwrap();
    let mut desired = core.desired_configuration()?.desired;
    desired.topology.components.clear();
    let removed = core
        .apply_desired_state(receipt.revision, "remove", desired)
        .await?;
    core.reconcile_desired_state().await?;
    assert!(old.observed().is_err());
    let error = core
        .apply_desired_state(removed.revision, "import", saved.desired.clone())
        .await
        .unwrap_err();
    assert!(external(&error), "{error:#}");
    assert_eq!(
        core.create_managed_component(0, "create", definition(&factory))
            .await?,
        receipt
    );
    assert!(core.computation_component(id.as_str()).is_err());
    let restored = core
        .restore_configuration_snapshot("saved", removed.revision, "restore")
        .await?;
    core.reconcile_desired_state().await?;
    assert_eq!(core.component_by_name("managed")?.id(), &id);
    assert!(old.observed().is_err());
    let restored_handle = core.computation_component(id.as_str())?;
    let before_rename = factory.creations.load(Ordering::SeqCst);
    core.computation_control()?
        .rename_component(&id, name("owner-routed"))
        .await?;
    let renamed = core.desired_configuration()?;
    assert!(renamed.revision > restored.revision);
    assert_eq!(
        renamed.desired.topology.components[0].name,
        name("owner-routed")
    );
    assert_eq!(
        restored_handle.generation(),
        core.component_by_name("owner-routed")?.generation()
    );
    assert_eq!(factory.creations.load(Ordering::SeqCst), before_rename);
    let mut desired = core.desired_configuration()?.desired;
    desired.topology.components[0].name = name("renamed");
    let before_rename = factory.creations.load(Ordering::SeqCst);
    core.apply_desired_state(renamed.revision, "rename", desired)
        .await?;
    core.reconcile_desired_state().await?;
    assert_eq!(factory.creations.load(Ordering::SeqCst), before_rename);
    core.shutdown().await?;
    let restarted = managed(store, factory.clone()).await?;
    assert!(!restarted.allow_external_component_ids());
    assert_eq!(restarted.component_by_name("renamed")?.id(), &id);
    assert!(factory.creations.load(Ordering::SeqCst) >= 3);
    let mut external_definition = definition(&factory);
    external_definition.id = Some(ComponentId::generate());
    assert!(external(
        &restarted
            .create_managed_component(
                restarted.desired_configuration()?.revision,
                "external",
                external_definition
            )
            .await
            .unwrap_err()
    ));
    restarted.shutdown().await?;
    Ok(())
}

#[test]
fn identity_authoring_is_explicit_and_untrusted_flags_are_rejected() -> anyhow::Result<()> {
    let id = ComponentId::generate();
    let input: drasi_lib::QueryInputConfig =
        serde_json::from_value(serde_json::json!(id.to_string()))?;
    assert_eq!(input.component.as_name().unwrap().as_str(), id.as_str());
    let explicit: drasi_lib::QueryInputConfig =
        serde_json::from_value(serde_json::json!({"id": id}))?;
    assert_eq!(explicit.component.as_id(), Some(&id));
    assert!(
        serde_json::from_value::<drasi_lib::QueryConfig>(serde_json::json!({
            "name":"query", "query":"MATCH (n) RETURN n", "trusted":true
        }))
        .is_err()
    );
    let mut old = serde_json::to_value(DesiredInstance::default())?;
    old["version"] = 1.into();
    assert!(serde_json::from_value::<DesiredInstance>(old)?
        .normalized()
        .is_err());
    Ok(())
}
