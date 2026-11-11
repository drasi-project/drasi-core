// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Context;
use async_trait::async_trait;
use drasi_lib::{computation::v1::*, management::*, DrasiLib, Query};
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

struct Service(ComponentDescriptor);
#[async_trait]
impl ComputationComponent for Service {
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
impl ComputationService for Service {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

struct Factory {
    descriptor: FactoryDescriptor,
    core: Mutex<Weak<DrasiLib>>,
    created: AtomicUsize,
}

impl Factory {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("fixture/batch-service", "1")
                    .unwrap(),
                role: ComponentRole::Service,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::new(),
                    allow_additional: true,
                },
                dependencies: BTreeMap::new(),
            },
            core: Mutex::new(Weak::new()),
            created: AtomicUsize::new(0),
        })
    }

    fn definition(&self, name: &str) -> ComponentDefinition {
        ComponentDefinition {
            name: ComponentName::try_new(name).unwrap(),
            id: None,
            implementation: self.descriptor.implementation.clone(),
            configuration: serde_json::json!({"business_key":"00000000-0000-4000-8000-000000000001"}),
            dependencies: BTreeMap::new(),
            streams: BTreeMap::new(),
            lifecycle: LifecyclePolicy { auto_start: false },
        }
    }
}

#[async_trait]
impl ComponentFactory for Factory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn specification_from_config(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
    ) -> anyhow::Result<ComponentSpecification> {
        Ok(ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(id, vec![])?,
            role: ComponentRole::Service,
            completion: None,
            implementation: self.descriptor.implementation.clone(),
            configuration_version: 1,
            configuration: configuration
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| {
                    (
                        Arc::from(key.as_str()),
                        ConfigurationValue::Literal(value.clone()),
                    )
                })
                .collect(),
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
        if let Some(core) = self.core.lock().unwrap().upgrade() {
            let saved = core.desired_configuration().unwrap();
            assert!(saved
                .desired
                .topology
                .components
                .iter()
                .any(|node| node.descriptor.id() == &context.component_id));
        }
        self.created.fetch_add(1, Ordering::SeqCst);
        Ok(ConstructedComponent::service(Box::new(Service(
            context.specification.descriptor.clone(),
        ))))
    }
}

fn application_source(id: ComponentId) -> anyhow::Result<ApplicationSource> {
    Ok(ApplicationSource::new(
        id.to_string(),
        ApplicationSourceConfig {
            properties: Default::default(),
            durability: None,
        },
    )?
    .0)
}

fn bound_batch(
    ids: &mut ComponentIds,
    factory: Arc<Factory>,
    name: &str,
) -> GraphResult<ComponentBatch> {
    let id = ids.allocate()?;
    let spec = factory
        .specification_from_config(id, serde_json::json!({}))
        .map_err(|error| GraphError::Topology {
            reason: error.to_string(),
        })?;
    ComponentBatch::builder()
        .component(ComponentName::try_new(name)?, spec, factory)
        .build()
}

#[tokio::test]
async fn builder_accepts_one_mixed_batch_before_any_constructor() -> anyhow::Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    let factory = Factory::new();
    let native = factory.clone();
    let invalid = DrasiLib::builder()
        .with_source_factory("duplicate", false, move |id| {
            called.fetch_add(1, Ordering::SeqCst);
            application_source(id)
        })
        .with_component_batch_factory(move |ids| bound_batch(ids, native, "duplicate"))
        .build()
        .await;
    assert!(invalid.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(factory.created.load(Ordering::SeqCst), 0);

    let called = calls.clone();
    let native = factory.clone();
    let core = DrasiLib::builder()
        .with_source_factory("orders", false, move |id| {
            called.fetch_add(1, Ordering::SeqCst);
            application_source(id)
        })
        .with_component_batch_factory(move |ids| bound_batch(ids, native, "native"))
        .build()
        .await?;
    settle(&core).await?;
    core.component_by_name("orders")?.wait_created().await?;
    core.component_by_name("native")?.wait_created().await?;
    let saved = core.desired_configuration()?;
    assert_eq!(saved.revision, 1);
    assert_eq!(saved.desired.ordinary.len(), 1);
    assert_eq!(saved.desired.topology.components.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(factory.created.load(Ordering::SeqCst), 1);
    assert!(!core.allow_external_component_ids());
    assert!(!core.configuration_is_persistent());
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn ordinary_creation_commits_before_construction_and_preserves_manual_lifecycle(
) -> anyhow::Result<()> {
    let core = Arc::new(DrasiLib::builder().build().await?);
    core.start().await?;
    let inspect = Arc::downgrade(&core);
    let source = core
        .create_source("manual", false, move |id| {
            let core = inspect.upgrade().unwrap();
            let saved = core.desired_configuration()?;
            assert_eq!(saved.revision, 1);
            assert_eq!(saved.desired.ordinary.len(), 1);
            assert_eq!(saved.desired.ordinary[0].id, id);
            assert!(matches!(
                saved.desired.ordinary[0].component,
                OrdinaryComponent::ProcessLocal { .. }
            ));
            application_source(id)
        })
        .await?;
    source.wait_created().await?;
    source.start().await?;
    let automatic = core
        .create_source("automatic", true, application_source)
        .await?;
    tokio::time::timeout(Duration::from_secs(5), automatic.wait_started()).await??;
    automatic.stop().await?;
    source.rename(ComponentName::try_new("renamed")?).await?;
    settle(&core).await?;
    assert_eq!(source.observed()?.lifecycle, ComponentLifecycle::Running);
    assert_eq!(automatic.observed()?.lifecycle, ComponentLifecycle::Stopped);
    assert!(core.management_status().await?.converged);
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn historical_opaque_bindings_cannot_adopt_a_replacement_object() -> anyhow::Result<()> {
    for external in [false, true] {
        let core = DrasiLib::builder()
            .with_allow_external_component_ids(external)
            .build()
            .await?;
        let old = core
            .create_source("source", false, application_source)
            .await?;
        old.wait_created().await?;
        core.snapshot_desired_configuration("old-object").await?;
        core.remove_source_by_id(old.id(), false).await?;
        let replacement = if external {
            core.add_source("replacement", application_source(old.id().clone())?)
                .await?;
            core.component_by_name("replacement")?
        } else {
            core.create_source("replacement", false, application_source)
                .await?
        };
        replacement.wait_created().await?;
        let current = core.desired_configuration()?;
        let error = core
            .restore_configuration_snapshot("old-object", current.revision, "restore-old")
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("process-local"), "{error:#}");
        assert_eq!(core.desired_configuration()?, current);
        assert_eq!(
            core.component_by_name("replacement")?.generation(),
            replacement.generation()
        );
        core.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn persistent_builders_reject_initial_factories_without_invoking_them() -> anyhow::Result<()>
{
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    let result = DrasiLib::builder()
        .with_configuration_store(Arc::new(DurableStore(Arc::new(DurableFixture::default()))))
        .with_component_batch_factory(move |_| {
            observed.store(true, Ordering::SeqCst);
            ComponentBatch::builder().build()
        })
        .build()
        .await;
    assert!(result.is_err());
    assert!(!called.load(Ordering::SeqCst));
    Ok(())
}
#[derive(Default)]
struct Stored {
    current: CommittedConfiguration,
    receipts: BTreeMap<String, (DesiredInstance, AcceptanceReceipt)>,
    snapshots: BTreeMap<String, CommittedConfiguration>,
}

#[derive(Default)]
struct DurableFixture(Mutex<Stored>);
struct DurableStore(Arc<DurableFixture>);

#[async_trait]
impl ConfigurationStore for DurableStore {
    fn guarantees(&self) -> ConfigurationStoreGuarantees {
        ConfigurationStoreGuarantees::DURABLE
    }
    async fn open(&self, _: &str) -> anyhow::Result<Arc<dyn ConfigurationSession>> {
        Ok(self.0.clone())
    }
}

#[async_trait]
impl ConfigurationSession for DurableFixture {
    async fn load(&self) -> anyhow::Result<CommittedConfiguration> {
        Ok(self.0.lock().unwrap().current.clone())
    }
    async fn commit(
        &self,
        expected: u64,
        request: &str,
        desired: &DesiredInstance,
        creation: Option<&AcceptedComponentCreation>,
        updates: &[AcceptedComponentUpdate],
    ) -> anyhow::Result<AcceptanceReceipt> {
        let mut state = self.0.lock().unwrap();
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
                actual: state.current.revision,
            }
        );
        if state.current.desired != *desired {
            state.current = CommittedConfiguration {
                revision: expected + 1,
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
        Ok(receipt)
    }
    async fn complete_updates(
        &self,
        expected: u64,
        updates: &[ComponentUpdateCompletion],
    ) -> anyhow::Result<CommittedConfiguration> {
        let mut state = self.0.lock().unwrap();
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
    async fn receipt(&self, id: &str) -> anyhow::Result<Option<AcceptanceReceipt>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .receipts
            .get(id)
            .map(|(_, receipt)| receipt.clone()))
    }
    async fn snapshot(&self, name: &str) -> anyhow::Result<CommittedConfiguration> {
        let mut state = self.0.lock().unwrap();
        let saved = state.current.clone();
        state.snapshots.insert(name.into(), saved.clone());
        Ok(saved)
    }
    async fn load_snapshot(&self, name: &str) -> anyhow::Result<Option<CommittedConfiguration>> {
        Ok(self.0.lock().unwrap().snapshots.get(name).cloned())
    }
    async fn close(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

async fn core(
    factory: &Arc<Factory>,
    durable: bool,
    external: bool,
) -> anyhow::Result<Arc<DrasiLib>> {
    let mut factories = FactoryRegistry::default();
    factories.register(factory.clone())?;
    let mut builder = DrasiLib::builder()
        .with_component_factories(factories)
        .with_allow_external_component_ids(external);
    if durable {
        builder = builder
            .with_configuration_store(Arc::new(DurableStore(Arc::new(DurableFixture::default()))));
    }
    let core = Arc::new(builder.build().await?);
    *factory.core.lock().unwrap() = Arc::downgrade(&core);
    Ok(core)
}

async fn settle(core: &DrasiLib) -> anyhow::Result<()> {
    let status =
        tokio::time::timeout(Duration::from_secs(5), core.reconcile_desired_state()).await??;
    anyhow::ensure!(
        status.error.is_none() && status.resource_errors.is_empty(),
        "{status:?}"
    );
    for node in core.computation_control()?.observed().components.values() {
        anyhow::ensure!(node.failure.is_none(), "{:?}", node.failure);
    }
    Ok(())
}

fn native_batch(
    ids: &mut ComponentIds,
    factory: &Factory,
    label: &str,
) -> GraphResult<DesiredTopology> {
    let root = ids.allocate()?;
    let other = ids.allocate()?;
    let helper = ids.derive_owned(&root, "fixed-helper")?;
    let control_connection = (root.clone(), other.clone());
    let mut topology = DesiredInstance::default().topology;
    for (id, name) in [(root, label), (other, "other"), (helper, "helper")] {
        let specification = factory
            .specification_from_config(id, serde_json::json!({}))
            .map_err(|error| GraphError::Topology {
                reason: error.to_string(),
            })?;
        topology.components.push(DesiredComponent {
            name: ComponentName::try_new(name)?,
            descriptor: specification.descriptor.clone(),
            role: specification.role,
            completion: None,
            streams: BTreeMap::new(),
            lifecycle: LifecyclePolicy { auto_start: false },
            input_merge: InputMergePolicy::default(),
            construction: ComponentConstruction::Factory(specification),
        });
    }
    topology.control_connections.push(control_connection);
    Ok(topology)
}

struct Resources(Arc<Factory>, AtomicUsize);

#[async_trait]
impl ManagementResourceResolver for Resources {
    async fn resolve(
        &self,
        _: &str,
        _: &str,
        specification: &ResourceSpecification,
        _: &serde_json::Value,
    ) -> anyhow::Result<ResourceHandle> {
        let core = self.0.core.lock().unwrap().upgrade().unwrap();
        let committed = core.desired_configuration()?;
        assert_eq!(committed.desired.topology.components.len(), 3);
        assert_eq!(committed.desired.topology.resources.len(), 2);
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(ResourceHandle::new(specification.role, Arc::new(())))
    }

    async fn resolve_with_dependencies(
        &self,
        instance: &str,
        graph: &str,
        specification: &ResourceSpecification,
        configuration: &serde_json::Value,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<ResourceHandle> {
        if specification.id.as_str() == "second" {
            assert!(dependencies.contains_key(&ResourceId::try_new("first")?));
        }
        self.resolve(instance, graph, specification, configuration)
            .await
    }
}

#[tokio::test]
async fn native_resources_and_connections_are_accepted_with_the_complete_group(
) -> anyhow::Result<()> {
    let factory = Factory::new();
    let resources = Arc::new(Resources(factory.clone(), AtomicUsize::new(0)));
    let mut factories = FactoryRegistry::default();
    factories.register(factory.clone())?;
    let core = Arc::new(
        DrasiLib::builder()
            .with_component_factories(factories)
            .with_management_resources(resources.clone())
            .build()
            .await?,
    );
    *factory.core.lock().unwrap() = Arc::downgrade(&core);
    let builder = factory.clone();
    core.create_managed_components(0, "complete-group", move |ids| {
        let mut topology = native_batch(ids, &builder, "root")?;
        for name in ["first", "second"] {
            let id = ResourceId::try_new(name)?;
            topology.resources.push(ResourceSpecification {
                id: id.clone(),
                role: ResourceRole::Component,
                ownership: ResourceOwnership::Borrowed,
                binding: name.into(),
            });
            topology
                .resource_configurations
                .insert(id, serde_json::json!({"kind":"fixture"}));
        }
        topology.resource_dependencies.insert(
            ResourceId::try_new("second")?,
            BTreeMap::from([(ResourceId::try_new("first")?, ResourceRole::Component)]),
        );
        Ok(topology)
    })
    .await?;
    settle(&core).await?;
    assert_eq!(resources.1.load(Ordering::SeqCst), 2);
    assert_eq!(
        core.desired_configuration()?
            .desired
            .topology
            .control_connections
            .len(),
        1
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn scoped_batches_keep_all_assignments_after_deletion_in_both_stores() -> anyhow::Result<()> {
    for durable in [false, true] {
        let factory = Factory::new();
        let core = core(&factory, durable, false).await?;
        assert!(core.has_managed_configuration());
        assert_eq!(core.configuration_is_persistent(), durable);
        let builder = factory.clone();
        let receipt = core
            .create_managed_components(0, "batch", move |ids| native_batch(ids, &builder, "root"))
            .await?;
        assert_eq!(receipt.durable, durable);
        assert_eq!(receipt.creation.as_ref().unwrap().components.len(), 3);
        assert_eq!(receipt.creation.as_ref().unwrap().allocations.len(), 2);
        settle(&core).await?;
        assert_eq!(factory.created.load(Ordering::SeqCst), 3);
        let saved = core.snapshot_desired_configuration("before-delete").await?;
        let removed = core
            .apply_desired_state(receipt.revision, "delete", DesiredInstance::default())
            .await?;
        settle(&core).await?;
        let builder = factory.clone();
        let retry = core
            .create_managed_components(0, "batch", move |ids| native_batch(ids, &builder, "root"))
            .await?;
        assert_eq!(retry, receipt);
        assert!(core
            .desired_configuration()?
            .desired
            .topology
            .components
            .is_empty());
        assert_eq!(factory.created.load(Ordering::SeqCst), 3);
        let builder = factory.clone();
        assert!(core
            .create_managed_components(0, "batch", move |ids| native_batch(
                ids, &builder, "changed"
            ))
            .await
            .is_err());
        assert!(core
            .apply_desired_state(removed.revision, "copied", saved.desired)
            .await
            .is_err());
        core.restore_configuration_snapshot("before-delete", removed.revision, "restore")
            .await?;
        settle(&core).await?;
        for assignment in &receipt.creation.as_ref().unwrap().components {
            assert_eq!(
                core.component_by_name(assignment.name.as_str())?.id(),
                &assignment.id
            );
        }
        core.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn general_authoring_validates_all_new_members_and_single_creation_is_a_batch(
) -> anyhow::Result<()> {
    let factory = Factory::new();
    let core = core(&factory, false, false).await?;
    let request = ConfigurationRequest::adding([
        NewComponentDefinition::Native(factory.definition("same")),
        NewComponentDefinition::Native(factory.definition("same")),
    ]);
    assert!(core
        .apply_configuration(0, "conflict", request)
        .await
        .is_err());
    assert_eq!(factory.created.load(Ordering::SeqCst), 0);
    assert_eq!(core.desired_configuration()?.revision, 0);
    let single = factory.definition("one");
    let receipt = core
        .create_managed_component(0, "single", single.clone())
        .await?;
    settle(&core).await?;
    let retry = core
        .apply_configuration(
            0,
            "single",
            ConfigurationRequest::adding([NewComponentDefinition::Native(single)]),
        )
        .await?;
    assert_eq!(retry, receipt);
    let assigned = &receipt.creation.as_ref().unwrap().components[0];
    assert_eq!(core.component_by_name("one")?.id(), &assigned.id);
    let spec = &core.desired_configuration()?.desired.topology.components[0].construction;
    let ComponentConstruction::Factory(spec) = spec else {
        unreachable!()
    };
    assert_eq!(
        spec.configuration["business_key"],
        ConfigurationValue::Literal("00000000-0000-4000-8000-000000000001".into())
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn explicit_ids_reject_by_default_and_are_preserved_exactly_when_opted_in(
) -> anyhow::Result<()> {
    for external in [false, true] {
        let factory = Factory::new();
        let core = core(&factory, false, external).await?;
        let chosen = ComponentId::generate();
        let mut definition = factory.definition("chosen");
        definition.id = Some(chosen.clone());
        let result = core
            .create_managed_component(0, "explicit", definition)
            .await;
        if external {
            let receipt = result?;
            assert_eq!(receipt.creation.unwrap().components[0].id, chosen);
            settle(&core).await?;
            assert_eq!(core.component_by_name("chosen")?.id(), &chosen);
        } else {
            assert!(matches!(result.unwrap_err().downcast_ref::<GraphError>(),
                Some(GraphError::ExternalComponentId { component }) if component == &chosen));
            assert_eq!(factory.created.load(Ordering::SeqCst), 0);
        }
        core.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn conflicting_batch_bindings_are_rejected_before_construction() -> anyhow::Result<()> {
    let left = Factory::new();
    let right = Factory::new();
    let first = left.clone();
    let second = right.clone();
    let result = DrasiLib::builder()
        .with_component_batch_factory(move |ids| bound_batch(ids, first, "first"))
        .with_component_batch_factory(move |ids| bound_batch(ids, second, "second"))
        .build()
        .await;
    assert!(result.is_err());
    assert_eq!(left.created.load(Ordering::SeqCst), 0);
    assert_eq!(right.created.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn process_local_update_rejection_preserves_identity_and_existing_query_inputs(
) -> anyhow::Result<()> {
    let core = DrasiLib::builder()
        .with_startup_timeout_secs(5)
        .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
        .build()
        .await?;
    let input = Arc::new(Mutex::new(None));
    let capture_input = input.clone();
    let original = core
        .create_source("data", true, move |id| {
            let (source, handle) = ApplicationSource::new(
                id.to_string(),
                ApplicationSourceConfig {
                    properties: Default::default(),
                    durability: None,
                },
            )?;
            *capture_input.lock().unwrap() = Some(handle);
            Ok(source)
        })
        .await?;
    original.wait_created().await?;
    let input = input.lock().unwrap().take().unwrap();
    let query = core
        .add_query_with_handle(
            Query::cypher("view")
                .query("MATCH (n:Item) RETURN n.value AS value")
                .from_input("data")
                .enable_bootstrap(false)
                .build()?,
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(5), query.wait_created()).await??;
    let rejected = core
        .update_source_by_id(
            original.id(),
            OrdinaryComponentRecord {
                id: original.id().clone(),
                name: ComponentName::try_new("data")?,
                auto_start: true,
                component: OrdinaryComponent::Source {
                    kind: "application".into(),
                    configuration: serde_json::json!({}),
                },
            },
        )
        .await;
    assert!(
        rejected.is_err(),
        "a creation callback is not a registered update factory"
    );
    let current = core.component_by_name("data")?;
    assert_eq!(current.id(), original.id());
    assert_eq!(current.generation(), original.generation());
    tokio::time::timeout(
        Duration::from_secs(5),
        core.computation_component(query.id().as_str())?
            .wait_created(),
    )
    .await??;
    assert_eq!(
        core.get_query_config_by_id(query.id()).await?.inputs[0]
            .component
            .as_id(),
        Some(original.id())
    );
    let output = Arc::new(Mutex::new(None));
    let capture = output.clone();
    let consumer = core
        .create_reaction(
            "consumer",
            vec![drasi_lib::config::ComponentReference::by_id(query.id().clone())],
            true,
            move |id, inputs| {
                let (reaction, handle) = drasi_reaction_application::ApplicationReaction::new(
                    id.to_string(),
                    inputs.iter().map(ToString::to_string).collect(),
                );
                *capture.lock().unwrap() = Some(handle);
                Ok(reaction)
            },
        )
        .await?;
    core.start().await?;
    consumer.wait_started().await?;
    let output = output.lock().unwrap().take().unwrap();
    let mut results = output.take_receiver().await.unwrap();
    input
        .send_node_insert(
            "row",
            vec!["Item"],
            PropertyMapBuilder::new().with_integer("value", 7).build(),
        )
        .await?;
    let result = tokio::time::timeout(Duration::from_secs(5), results.recv())
        .await?
        .unwrap();
    assert_eq!(result.query_id, query.id().as_str());
    assert!(!core.allow_external_component_ids());
    core.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn removing_one_native_query_preserves_a_shared_outlet_for_its_peer() -> anyhow::Result<()> {
    let core = DrasiLib::builder()
        .with_query_factory(Arc::new(ContinuousQueryFactory::default()))
        .build()
        .await?;
    let pipeline = core.computation_pipeline()?;
    let left = Query::cypher("left")
        .query("MATCH (n) RETURN n")
        .enable_bootstrap(false)
        .auto_start(false)
        .build()?;
    let right = Query::cypher("right")
        .query("MATCH (n) RETURN n")
        .enable_bootstrap(false)
        .auto_start(false)
        .build()?;
    let receipt = core
        .create_components(move |ids| {
            let left_id = ids.allocate()?;
            let right_id = ids.allocate()?;
            pipeline
                .query(left.with_assigned_id(left_id))
                .query(right.with_assigned_id(right_id))
                .build_owned(ids)
        })
        .await
        .context("creating native peer queries")?;
    settle(&core)
        .await
        .context("realizing native peer queries")?;
    let left = core.component_by_name("left")?;
    let right = core.component_by_name("right")?;
    left.wait_created().await?;
    right.wait_created().await?;
    let assigned = receipt.creation.unwrap().components;
    assert!(assigned.iter().any(|entry| &entry.id == left.id()));
    core.remove_query_by_id(left.id())
        .await
        .context("removing left query")?;
    assert!(left.observed().is_err());
    core.computation_component(right.id().as_str())?
        .wait_created()
        .await?;
    assert_eq!(core.component_by_name("right")?.id(), right.id());
    core.remove_query_by_id(right.id())
        .await
        .context("removing right query")?;
    assert!(core
        .computation_control()?
        .desired_snapshot()
        .nodes
        .is_empty());
    core.shutdown().await?;
    Ok(())
}
